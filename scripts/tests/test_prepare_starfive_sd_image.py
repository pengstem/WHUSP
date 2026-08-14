from __future__ import annotations

import gzip
import json
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import prepare_starfive_sd_image as prepare


class PrepareStarFiveSdImageTests(unittest.TestCase):
    def test_parse_size_uses_binary_units(self) -> None:
        self.assertEqual(prepare.parse_size("3584MiB"), 3584 * 1024 * 1024)
        self.assertEqual(prepare.parse_size("2GiB"), 2 * 1024**3)
        self.assertEqual(prepare.parse_size("512"), 512)

    def test_raw_image_is_streamed_into_contiguous_verified_chunks(self) -> None:
        payload = bytes(range(256)) * 10  # 2560 bytes, five MMC blocks.
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source = root / "official.img"
            output = root / "tftp"
            source.write_bytes(payload)
            manifest_path = prepare.prepare_image(
                source,
                output,
                prefix="test-sd",
                chunk_size=1024,
                max_compressed_size=4096,
                compressor="python",
            )

            manifest = json.loads(manifest_path.read_text())
            self.assertEqual(manifest["schema"], prepare.SCHEMA)
            self.assertEqual(manifest["compression"]["compressor"], "python")
            self.assertEqual(manifest["image"]["size"], len(payload))
            self.assertEqual(
                [chunk["offset"] for chunk in manifest["chunks"]], [0, 1024, 2048]
            )
            self.assertEqual(
                [chunk["raw_size"] for chunk in manifest["chunks"]], [1024, 1024, 512]
            )
            rebuilt = bytearray()
            for chunk in manifest["chunks"]:
                with gzip.open(output / chunk["filename"], "rb") as encoded:
                    rebuilt.extend(encoded.read())
            self.assertEqual(bytes(rebuilt), payload)

    def test_gzip_source_does_not_need_a_materialized_raw_image(self) -> None:
        payload = b"WHUSP" * 1024
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source = root / "official.img.gz"
            with gzip.open(source, "wb") as output:
                output.write(payload)
            manifest_path = prepare.prepare_image(
                source,
                root / "tftp",
                prefix="gzip-sd",
                chunk_size=1024,
                max_compressed_size=4096,
                compressor="python",
            )
            manifest = json.loads(manifest_path.read_text())
            self.assertEqual(manifest["source"]["format"], "gzip")
            self.assertEqual(manifest["image"]["size"], len(payload))

    def test_unaligned_image_rolls_back_all_generated_files(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source = root / "bad.img"
            output = root / "tftp"
            source.write_bytes(b"x" * 513)
            with self.assertRaisesRegex(RuntimeError, "not aligned"):
                prepare.prepare_image(
                    source,
                    output,
                    prefix="bad-sd",
                    chunk_size=512,
                    max_compressed_size=4096,
                    compressor="python",
                )
            self.assertEqual(list(output.iterdir()), [])

    def test_existing_prepared_image_is_never_replaced(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source = root / "official.img"
            output = root / "tftp"
            source.write_bytes(b"x" * 512)
            prepare.prepare_image(
                source,
                output,
                prefix="same-sd",
                chunk_size=512,
                max_compressed_size=4096,
                compressor="python",
            )
            with self.assertRaises(FileExistsError):
                prepare.prepare_image(
                    source,
                    output,
                    prefix="same-sd",
                    chunk_size=512,
                    max_compressed_size=4096,
                    compressor="python",
                )


if __name__ == "__main__":
    unittest.main()
