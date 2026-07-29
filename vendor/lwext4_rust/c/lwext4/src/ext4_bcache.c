/*
 * Copyright (c) 2013 Grzegorz Kostka (kostka.grzegorz@gmail.com)
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 *
 * - Redistributions of source code must retain the above copyright
 *   notice, this list of conditions and the following disclaimer.
 * - Redistributions in binary form must reproduce the above copyright
 *   notice, this list of conditions and the following disclaimer in the
 *   documentation and/or other materials provided with the distribution.
 * - The name of the author may not be used to endorse or promote products
 *   derived from this software without specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
 * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
 * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
 * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
 * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
 * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
 * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
 * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
 * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
 * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */

/** @addtogroup lwext4
 * @{
 */
/**
 * @file  ext4_bcache.c
 * @brief Block cache allocator.
 */

#include <ext4_config.h>
#include <ext4_types.h>
#include <ext4_bcache.h>
#include <ext4_blockdev.h>
#include <ext4_debug.h>
#include <ext4_errno.h>

#include <string.h>
#include <stdlib.h>

static int ext4_bcache_lba_compare(struct ext4_buf *a, struct ext4_buf *b)
{
	 if (a->lba > b->lba)
		 return 1;
	 else if (a->lba < b->lba)
		 return -1;
	 return 0;
}

static int ext4_bcache_lru_compare(struct ext4_buf *a, struct ext4_buf *b)
{
	if (a->lru_id > b->lru_id)
		return 1;
	else if (a->lru_id < b->lru_id)
		return -1;
	return 0;
}

RB_GENERATE_INTERNAL(ext4_buf_lba, ext4_buf, lba_node,
		     ext4_bcache_lba_compare, static inline)
RB_GENERATE_INTERNAL(ext4_buf_lru, ext4_buf, lru_node,
			     ext4_bcache_lru_compare, static inline)

void ext4_bcache_index_lock_impl(struct ext4_bcache *bc)
{
	if (!bc || !bc->bdev || !bc->bdev->bdif->bcache_index_lock)
		return;

	int r = bc->bdev->bdif->bcache_index_lock(bc->bdev);
	ext4_assert(r == EOK);
}

void ext4_bcache_index_unlock_impl(struct ext4_bcache *bc)
{
	if (!bc || !bc->bdev || !bc->bdev->bdif->bcache_index_unlock)
		return;

	int r = bc->bdev->bdif->bcache_index_unlock(bc->bdev);
	ext4_assert(r == EOK);
}

void ext4_bcache_lba_lock_impl(struct ext4_bcache *bc, uint64_t lba)
{
	if (!bc || !bc->bdev || !bc->bdev->bdif->bcache_lba_lock)
		return;

	int r = bc->bdev->bdif->bcache_lba_lock(bc->bdev, lba);
	ext4_assert(r == EOK);
}

void ext4_bcache_lba_unlock_impl(struct ext4_bcache *bc, uint64_t lba)
{
	if (!bc || !bc->bdev || !bc->bdev->bdif->bcache_lba_unlock)
		return;

	int r = bc->bdev->bdif->bcache_lba_unlock(bc->bdev, lba);
	ext4_assert(r == EOK);
}

static uint64_t ext4_bcache_generation(struct ext4_bcache *bc)
{
	if (!bc || !bc->bdev || !bc->bdev->bdif->bcache_generation)
		return 0;

	return bc->bdev->bdif->bcache_generation(bc->bdev);
}

int ext4_bcache_init_dynamic(struct ext4_bcache *bc, uint32_t cnt,
			     uint32_t itemsize)
{
	ext4_assert(bc && cnt && itemsize);

	memset(bc, 0, sizeof(struct ext4_bcache));

	bc->cnt = cnt;
	bc->itemsize = itemsize;
	bc->ref_blocks = 0;
	bc->max_ref_blocks = 0;

	return EOK;
}

void ext4_bcache_cleanup(struct ext4_bcache *bc)
{
	struct ext4_buf *buf;
	while ((buf = ext4_bcache_reserve_lru(bc))) {
		uint64_t lba = buf->lba;
		ext4_bcache_lba_lock(bc, lba);
		if (!ext4_bcache_reserved_only(bc, buf)) {
			ext4_bcache_release_reservation(bc, buf);
			ext4_bcache_lba_unlock(bc, lba);
			continue;
		}

		if (ext4_block_flush_buf_locked(bc->bdev, buf) != EOK) {
			ext4_bcache_release_reservation(bc, buf);
			ext4_bcache_lba_unlock(bc, lba);
			break;
		}
		ext4_bcache_drop_reserved(bc, buf);
		ext4_bcache_lba_unlock(bc, lba);
	}
}

int ext4_bcache_fini_dynamic(struct ext4_bcache *bc)
{
	memset(bc, 0, sizeof(struct ext4_bcache));
	return EOK;
}

/**@brief:
 *
 *  This is ext4_bcache, the module handling basic buffer-cache stuff.
 *
 *  Buffers in a bcache are sorted by their LBA and stored in a
 *  RB-Tree(lba_root).
 *
 *  Bcache also maintains another RB-Tree(lru_root) right now, where
 *  buffers are sorted by their LRU id.
 *
 *  A singly-linked list is used to track those dirty buffers which are
 *  ready to be flushed. (Those buffers which are dirty but also referenced
 *  are not considered ready to be flushed.)
 *
 *  When a buffer is not referenced, it will be stored in both lba_root
 *  and lru_root, while it will only be stored in lba_root when it is
 *  referenced.
 */

static struct ext4_buf *
ext4_buf_alloc(struct ext4_bcache *bc, uint64_t lba)
{
	void *data;
	struct ext4_buf *buf;
	data = ext4_malloc(bc->itemsize);
	if (!data)
		return NULL;

	buf = ext4_calloc(1, sizeof(struct ext4_buf));
	if (!buf) {
		ext4_free(data);
		return NULL;
	}

	buf->lba = lba;
	buf->data = data;
	buf->bc = bc;
	buf->generation = 0;
	buf->detached = false;
	ext4_bcache_set_state(buf, BC_STATE_EMPTY);
	return buf;
}

static void ext4_buf_free(struct ext4_buf *buf)
{
	ext4_free(buf->data);
	ext4_free(buf);
}

static struct ext4_buf *
ext4_buf_lookup(struct ext4_bcache *bc, uint64_t lba)
{
	struct ext4_buf tmp = {
		.lba = lba
	};

	return RB_FIND(ext4_buf_lba, &bc->lba_root, &tmp);
}

static struct ext4_buf *
ext4_bcache_find_get_legacy(struct ext4_bcache *bc, struct ext4_block *b,
			    uint64_t lba)
{
	struct ext4_buf *buf = ext4_buf_lookup(bc, lba);
	if (buf) {
		if (!buf->refctr) {
			buf->lru_id = ++bc->lru_ctr;
			RB_REMOVE(ext4_buf_lru, &bc->lru_root, buf);
			if (buf->flags & (1 << BC_DIRTY))
				ext4_bcache_remove_dirty_node(bc, buf);
		}
		buf->refctr++;
		b->lb_id = lba;
		b->buf = buf;
		b->data = buf->data;
	}
	return buf;
}

static int ext4_bcache_alloc_legacy(struct ext4_bcache *bc,
				    struct ext4_block *b, bool *is_new)
{
	struct ext4_buf *buf = ext4_bcache_find_get_legacy(bc, b, b->lb_id);
	if (buf) {
		*is_new = false;
		return EOK;
	}

	buf = ext4_buf_alloc(bc, b->lb_id);
	if (!buf)
		return ENOMEM;
	RB_INSERT(ext4_buf_lba, &bc->lba_root, buf);
	bc->ref_blocks++;
	if (bc->max_ref_blocks < bc->ref_blocks)
		bc->max_ref_blocks = bc->ref_blocks;
	buf->refctr++;
	buf->lru_id = ++bc->lru_ctr;
	b->buf = buf;
	b->data = buf->data;
	*is_new = true;
	return EOK;
}

static void ext4_bcache_drop_buf_legacy(struct ext4_bcache *bc,
					struct ext4_buf *buf)
{
	if (buf->refctr) {
		ext4_dbg(DEBUG_BCACHE, DBG_WARN "Buffer is still referenced. "
				"lba: %" PRIu64 ", refctr: %" PRIu32 "\n",
				buf->lba, buf->refctr);
	} else {
		RB_REMOVE(ext4_buf_lru, &bc->lru_root, buf);
	}
	RB_REMOVE(ext4_buf_lba, &bc->lba_root, buf);
	if (buf->flags & (1 << BC_DIRTY))
		ext4_bcache_remove_dirty_node(bc, buf);
	ext4_buf_free(buf);
	bc->ref_blocks--;
}

static int ext4_bcache_free_legacy(struct ext4_bcache *bc,
				   struct ext4_block *b)
{
	struct ext4_buf *buf = b->buf;
	ext4_assert(bc && b && b->lb_id && buf && buf->refctr);
	buf->refctr--;
	if (!buf->refctr) {
		RB_INSERT(ext4_buf_lru, &bc->lru_root, buf);
		if ((buf->flags & (1 << BC_DIRTY)) &&
		    (buf->flags & (1 << BC_UPTODATE))) {
			if (bc->bdev->cache_write_back &&
			    !(buf->flags & (1 << BC_FLUSH)) &&
			    !(buf->flags & (1 << BC_TMP))) {
				ext4_bcache_insert_dirty_node(bc, buf);
			} else {
				ext4_block_flush_buf(bc->bdev, buf);
				buf->flags &= ~(1 << BC_FLUSH);
			}
		}
		if (!(buf->flags & (1 << BC_UPTODATE)) ||
		    (buf->flags & (1 << BC_TMP)))
			ext4_bcache_drop_buf_legacy(bc, buf);
	}
	b->lb_id = 0;
	b->buf = 0;
	b->data = 0;
	return EOK;
}

struct ext4_buf *ext4_buf_lowest_lru(struct ext4_bcache *bc)
{
	struct ext4_buf *buf;
	ext4_bcache_index_lock(bc);
	buf = RB_MIN(ext4_buf_lru, &bc->lru_root);
	ext4_bcache_index_unlock(bc);
	return buf;
}

static void ext4_bcache_drop_buf_locked(struct ext4_bcache *bc,
					struct ext4_buf *buf)
{
	ext4_assert(ext4_bcache_ref_count(buf) == 0);
	ext4_assert(!buf->detached);
	RB_REMOVE(ext4_buf_lru, &bc->lru_root, buf);

	RB_REMOVE(ext4_buf_lba, &bc->lba_root, buf);

	/*Forcibly drop dirty buffer.*/
	if (ext4_bcache_test_flag(buf, BC_DIRTY))
		ext4_bcache_remove_dirty_node(bc, buf);

	bc->ref_blocks--;
}

void ext4_bcache_drop_buf(struct ext4_bcache *bc, struct ext4_buf *buf)
{
	if (!bc->concurrent) {
		ext4_bcache_drop_buf_legacy(bc, buf);
		return;
	}
	uint64_t lba = buf->lba;
	bool drop = false;
	ext4_bcache_lba_lock(bc, lba);
	ext4_bcache_index_lock(bc);
	if (ext4_bcache_ref_count(buf)) {
		ext4_dbg(DEBUG_BCACHE, DBG_WARN "Buffer is still referenced. "
				"lba: %" PRIu64 ", refctr: %" PRIu32 "\n",
				buf->lba, ext4_bcache_ref_count(buf));
	} else {
		ext4_bcache_drop_buf_locked(bc, buf);
		drop = true;
	}
	ext4_bcache_index_unlock(bc);
	if (drop)
		ext4_buf_free(buf);
	ext4_bcache_lba_unlock(bc, lba);
}

void ext4_bcache_invalidate_buf(struct ext4_bcache *bc,
				struct ext4_buf *buf)
{
	uint64_t lba = buf->lba;
	ext4_bcache_lba_lock(bc, lba);
	ext4_bcache_index_lock(bc);
	buf->end_write = NULL;
	buf->end_write_arg = NULL;

	/* Clear both dirty and up-to-date flags. */
	if (ext4_bcache_test_flag(buf, BC_DIRTY))
		ext4_bcache_remove_dirty_node(bc, buf);

	ext4_bcache_clear_dirty(buf);
	ext4_bcache_index_unlock(bc);
	ext4_bcache_lba_unlock(bc, lba);
}

void ext4_bcache_invalidate_lba(struct ext4_bcache *bc,
				uint64_t from,
				uint32_t cnt)
{
	uint32_t i;
	for (i = 0; i < cnt; i++) {
		struct ext4_block block = EXT4_BLOCK_ZERO();
		uint64_t lba = from + i;
		if (lba < from)
			break;
		if (!ext4_bcache_find_get(bc, &block, lba))
			continue;
		ext4_bcache_invalidate_buf(bc, block.buf);
		ext4_bcache_free(bc, &block);
	}
}

struct ext4_buf *
ext4_bcache_find_get_locked(struct ext4_bcache *bc, struct ext4_block *b,
			    uint64_t lba)
{
	if (!bc->concurrent)
		return ext4_bcache_find_get_legacy(bc, b, lba);
	struct ext4_buf *buf;
	ext4_bcache_index_lock(bc);
	buf = ext4_buf_lookup(bc, lba);
	if (buf) {
		uint64_t generation = ext4_bcache_generation(bc);
		if (buf->generation != generation) {
			if (ext4_bcache_ref_count(buf)) {
				/* Keep the old payload immutable for existing readers,
				 * but detach it so the new generation allocates a
				 * different buffer. */
				ext4_assert(!ext4_bcache_test_flag(buf, BC_DIRTY));
				RB_REMOVE(ext4_buf_lba, &bc->lba_root, buf);
				buf->detached = true;
				buf = NULL;
			} else {
				/* With no owner, the allocation can be refilled in place
				 * after the normal LRU-to-referenced transition below. */
				if (ext4_bcache_test_flag(buf, BC_DIRTY))
					ext4_bcache_remove_dirty_node(bc, buf);
				ext4_bcache_clear_dirty(buf);
				buf->generation = generation;
			}
		}
	}
	if (buf) {
		/* If buffer is not referenced. */
		if (!ext4_bcache_ref_count(buf)) {
			/* Assign new value to LRU id and increment LRU counter
			 * by 1*/
			buf->lru_id = ++bc->lru_ctr;
			RB_REMOVE(ext4_buf_lru, &bc->lru_root, buf);
			if (ext4_bcache_test_flag(buf, BC_DIRTY))
				ext4_bcache_remove_dirty_node(bc, buf);

		}

		ext4_bcache_inc_ref(buf);

		b->lb_id = lba;
		b->buf = buf;
		b->data = buf->data;
	}
	ext4_bcache_index_unlock(bc);
	return buf;
}

struct ext4_buf *
ext4_bcache_find_get(struct ext4_bcache *bc, struct ext4_block *b,
			     uint64_t lba)
{
	if (!bc->concurrent)
		return ext4_bcache_find_get_legacy(bc, b, lba);
	struct ext4_buf *buf;
	ext4_bcache_lba_lock(bc, lba);
	buf = ext4_bcache_find_get_locked(bc, b, lba);
	ext4_bcache_lba_unlock(bc, lba);
	return buf;
}

struct ext4_buf *
ext4_bcache_find_get_uptodate(struct ext4_bcache *bc, struct ext4_block *b,
			      uint64_t lba)
{
	if (!bc->concurrent)
		return ext4_bcache_find_get_legacy(bc, b, lba);

	struct ext4_buf *buf;
	ext4_bcache_index_lock(bc);
	buf = ext4_buf_lookup(bc, lba);
	if (!buf || buf->detached ||
	    buf->generation != ext4_bcache_generation(bc) ||
	    ext4_bcache_get_state(buf) != BC_STATE_UPTODATE ||
	    !ext4_bcache_test_flag(buf, BC_UPTODATE) ||
	    ext4_bcache_test_flag(buf, BC_DIRTY) ||
	    ext4_bcache_test_flag(buf, BC_FLUSH) ||
	    ext4_bcache_test_flag(buf, BC_TMP) || buf->end_write) {
		buf = NULL;
	} else {
		if (!ext4_bcache_ref_count(buf)) {
			buf->lru_id = ++bc->lru_ctr;
			RB_REMOVE(ext4_buf_lru, &bc->lru_root, buf);
		}
		ext4_bcache_inc_ref(buf);
		b->lb_id = lba;
		b->buf = buf;
		b->data = buf->data;
	}
	ext4_bcache_index_unlock(bc);
	return buf;
}

int ext4_bcache_alloc_locked(struct ext4_bcache *bc, struct ext4_block *b,
			     bool *is_new)
{
	if (!bc->concurrent)
		return ext4_bcache_alloc_legacy(bc, b, is_new);
	/* Try to search the buffer with exaxt LBA. */
	struct ext4_buf *buf = ext4_bcache_find_get_locked(bc, b, b->lb_id);
	if (buf) {
		*is_new = false;
		return EOK;
	}

	/* We need to allocate one buffer.*/
	buf = ext4_buf_alloc(bc, b->lb_id);
	if (!buf)
		return ENOMEM;

	ext4_bcache_index_lock(bc);
	ext4_assert(!ext4_buf_lookup(bc, b->lb_id));
	buf->generation = ext4_bcache_generation(bc);
	RB_INSERT(ext4_buf_lba, &bc->lba_root, buf);
	/* One more buffer in bcache now. :-) */
	bc->ref_blocks++;

	/*Calc ref blocks max depth*/
	if (bc->max_ref_blocks < bc->ref_blocks)
		bc->max_ref_blocks = bc->ref_blocks;


	ext4_bcache_inc_ref(buf);
	/* Assign new value to LRU id and increment LRU counter
	 * by 1*/
	buf->lru_id = ++bc->lru_ctr;

	b->buf = buf;
	b->data = buf->data;
	ext4_bcache_index_unlock(bc);

	*is_new = true;
	return EOK;
}

int ext4_bcache_alloc(struct ext4_bcache *bc, struct ext4_block *b,
			      bool *is_new)
{
	if (!bc->concurrent)
		return ext4_bcache_alloc_legacy(bc, b, is_new);
	int r;
	ext4_bcache_lba_lock(bc, b->lb_id);
	r = ext4_bcache_alloc_locked(bc, b, is_new);
	ext4_bcache_lba_unlock(bc, b->lb_id);
	return r;
}

static void ext4_bcache_zero_ref_locked(struct ext4_bcache *bc,
					struct ext4_buf *buf)
{
	RB_INSERT(ext4_buf_lru, &bc->lru_root, buf);
	if (ext4_bcache_test_flag(buf, BC_DIRTY) &&
	    ext4_bcache_test_flag(buf, BC_UPTODATE) &&
	    bc->bdev->cache_write_back &&
	    !ext4_bcache_test_flag(buf, BC_FLUSH) &&
	    !ext4_bcache_test_flag(buf, BC_TMP))
		ext4_bcache_insert_dirty_node(bc, buf);
}

int ext4_bcache_free(struct ext4_bcache *bc, struct ext4_block *b)
{
	if (!bc->concurrent)
		return ext4_bcache_free_legacy(bc, b);
	struct ext4_buf *buf = b->buf;
	uint64_t lba;
	bool flush = false;
	bool drop = false;
	int r = EOK;

	ext4_assert(bc && b);

	/*Check if valid.*/
	ext4_assert(b->lb_id);
	lba = b->lb_id;

	/*Block should have a valid pointer to ext4_buf.*/
	ext4_assert(buf);

	/* Immutable cache hits need only serialize the refcount/LRU transition.
	 * The LBA sleeping lock is reserved for loading, invalidation, dirty
	 * writeback and buffer-state publication. Detached buffers are no longer
	 * discoverable, so their last immutable reference may retire them here. */
	bool clean_fast = false;
	bool detached_drop = false;
	ext4_bcache_index_lock(bc);
	if (ext4_bcache_get_state(buf) == BC_STATE_UPTODATE &&
	    ext4_bcache_test_flag(buf, BC_UPTODATE) &&
	    !ext4_bcache_test_flag(buf, BC_DIRTY) &&
	    !ext4_bcache_test_flag(buf, BC_FLUSH) &&
	    !ext4_bcache_test_flag(buf, BC_TMP) && !buf->end_write) {
		ext4_assert(ext4_bcache_ref_count(buf));
		if (!ext4_bcache_dec_ref(buf)) {
			if (buf->detached) {
				bc->ref_blocks--;
				detached_drop = true;
			} else {
				ext4_bcache_zero_ref_locked(bc, buf);
			}
		}
		clean_fast = true;
	}
	ext4_bcache_index_unlock(bc);
	if (clean_fast) {
		if (detached_drop)
			ext4_buf_free(buf);
		b->lb_id = 0;
		b->buf = 0;
		b->data = 0;
		return EOK;
	}

	ext4_bcache_lba_lock(bc, lba);
	ext4_bcache_index_lock(bc);
	/*Check if someone don't try free unreferenced block cache.*/
	ext4_assert(ext4_bcache_ref_count(buf));

	/*Just decrease reference counter*/
	if (!ext4_bcache_dec_ref(buf)) {
		if (buf->detached) {
			ext4_assert(!ext4_bcache_test_flag(buf, BC_DIRTY));
			bc->ref_blocks--;
			drop = true;
		} else if (ext4_bcache_test_flag(buf, BC_DIRTY) &&
		    ext4_bcache_test_flag(buf, BC_UPTODATE) &&
		    (!bc->bdev->cache_write_back ||
		     ext4_bcache_test_flag(buf, BC_FLUSH) ||
		     ext4_bcache_test_flag(buf, BC_TMP))) {
			/* Keep one private reservation while the index lock is
			 * dropped for writeback. */
			ext4_bcache_inc_ref(buf);
			flush = true;
		} else {
			ext4_bcache_zero_ref_locked(bc, buf);
			if (!ext4_bcache_test_flag(buf, BC_UPTODATE) ||
			    ext4_bcache_test_flag(buf, BC_TMP)) {
				ext4_bcache_drop_buf_locked(bc, buf);
				drop = true;
			}
		}
	}
	ext4_bcache_index_unlock(bc);

	if (flush) {
		r = ext4_block_flush_buf_locked(bc->bdev, buf);
		ext4_bcache_index_lock(bc);
		ext4_assert(ext4_bcache_ref_count(buf) == 1);
		ext4_bcache_dec_ref(buf);
		ext4_bcache_clear_flag(buf, BC_FLUSH);
		ext4_bcache_zero_ref_locked(bc, buf);
		if (r == EOK && (!ext4_bcache_test_flag(buf, BC_UPTODATE) ||
				 ext4_bcache_test_flag(buf, BC_TMP))) {
			ext4_bcache_drop_buf_locked(bc, buf);
			drop = true;
		}
		ext4_bcache_index_unlock(bc);
	}

	if (drop)
		ext4_buf_free(buf);
	ext4_bcache_lba_unlock(bc, lba);

	b->lb_id = 0;
	b->buf = 0;
	b->data = 0;

	return r;
}

bool ext4_bcache_is_full(struct ext4_bcache *bc)
{
	if (!bc->concurrent)
		return bc->cnt <= bc->ref_blocks;
	bool full;
	ext4_bcache_index_lock(bc);
	full = bc->cnt <= bc->ref_blocks;
	ext4_bcache_index_unlock(bc);
	return full;
}

struct ext4_buf *ext4_bcache_reserve_lru(struct ext4_bcache *bc)
{
	struct ext4_buf *buf;
	ext4_bcache_index_lock(bc);
	buf = RB_MIN(ext4_buf_lru, &bc->lru_root);
	if (buf) {
		ext4_assert(ext4_bcache_ref_count(buf) == 0);
		RB_REMOVE(ext4_buf_lru, &bc->lru_root, buf);
		if (ext4_bcache_test_flag(buf, BC_DIRTY))
			ext4_bcache_remove_dirty_node(bc, buf);
		ext4_bcache_inc_ref(buf);
	}
	ext4_bcache_index_unlock(bc);
	return buf;
}

bool ext4_bcache_reserved_only(struct ext4_bcache *bc, struct ext4_buf *buf)
{
	bool only;
	ext4_bcache_index_lock(bc);
	only = ext4_bcache_ref_count(buf) == 1 &&
	       ext4_buf_lookup(bc, buf->lba) == buf;
	ext4_bcache_index_unlock(bc);
	return only;
}

void ext4_bcache_release_reservation(struct ext4_bcache *bc,
				     struct ext4_buf *buf)
{
	ext4_bcache_index_lock(bc);
	ext4_assert(ext4_bcache_ref_count(buf));
	if (!ext4_bcache_dec_ref(buf))
		ext4_bcache_zero_ref_locked(bc, buf);
	ext4_bcache_index_unlock(bc);
}

void ext4_bcache_drop_reserved(struct ext4_bcache *bc, struct ext4_buf *buf)
{
	ext4_bcache_index_lock(bc);
	ext4_assert(ext4_bcache_ref_count(buf) == 1);
	ext4_bcache_dec_ref(buf);
	RB_REMOVE(ext4_buf_lba, &bc->lba_root, buf);
	if (ext4_bcache_test_flag(buf, BC_DIRTY))
		ext4_bcache_remove_dirty_node(bc, buf);
	bc->ref_blocks--;
	ext4_bcache_index_unlock(bc);
	ext4_buf_free(buf);
}

bool ext4_bcache_peek_dirty_lba(struct ext4_bcache *bc, uint64_t *lba)
{
	struct ext4_buf *buf;
	ext4_bcache_index_lock(bc);
	buf = SLIST_FIRST(&bc->dirty_list);
	if (buf)
		*lba = buf->lba;
	ext4_bcache_index_unlock(bc);
	return buf != NULL;
}

bool ext4_bcache_block_is_clean_exclusive(struct ext4_bcache *bc,
					  struct ext4_block *b)
{
	bool eligible;
	ext4_bcache_lba_lock(bc, b->lb_id);
	ext4_bcache_index_lock(bc);
	eligible = ext4_bcache_ref_count(b->buf) == 1 &&
		   !ext4_bcache_test_flag(b->buf, BC_DIRTY) &&
		   ext4_bcache_test_flag(b->buf, BC_UPTODATE);
	ext4_bcache_index_unlock(bc);
	ext4_bcache_lba_unlock(bc, b->lb_id);
	return eligible;
}


/**
 * @}
 */
