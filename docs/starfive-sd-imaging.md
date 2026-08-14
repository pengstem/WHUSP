# VisionFive 2 无读卡器烧卡与清理手册

本文覆盖没有 USB/microSD 读卡器时的决赛流程：镜像留在宿主机，VisionFive 2
从 TFTP 接收受限大小的 gzip 分块，U-Boot 使用 `gzwrite` 直接写入板载 microSD。
同一套控制面还能从 RAM 启动维护 FIT，列出或删除 BuildStorm 临时运行目录。

以下工具不会修改 QSPI，也不会执行 `saveenv`。普通 dry-run 和 preflight 不会写
microSD；只有 `flash_starfive_sd.py --execute` 和 delete 维护 FIT 会修改卡上内容。

## 1. 工具与安全边界

| 工具 | 默认行为 | 会写 microSD 的条件 |
| --- | --- | --- |
| `scripts/prepare_starfive_sd_image.py` | 在宿主机生成 gzip 分块、CRC/SHA-256 清单 | 永不写板载卡 |
| `scripts/flash_starfive_sd.py` | 校验本地分块并打印 U-Boot 写入计划 | 同时给出 `--execute`、完整镜像 SHA-256、预期卡名和容量 |
| `scripts/build_starfive_sd_maintenance.py` | 构建只列目录的维护 FIT | 构建 delete FIT 时还必须给出口令，运行该 FIT 后才删除 |
| `scripts/run_starfive_cagent.py --mode sd-maintenance` | 校验 FIT、卡身份并从 RAM 启动 | 取决于 FIT 内固化的是 list 还是 delete 动作 |

烧录工具逐块验证四层事实：宿主机 gzip SHA-256、TFTP 字节数、板上 gzip
头 `1f 8b 08`、`gzwrite` 最终解压字节数和 CRC32。任意一层不一致都会停止。

当前实板验证过的 U-Boot 是 `2021.10`，`gzwrite` 对超过 2 GiB 的压缩输入存在
边界问题。准备工具默认按 3.5 GiB 原始数据分块，并强制每个 gzip 文件低于
`0x7f000000` 字节。不要把完整的 2 GiB 以上 gzip 直接交给 U-Boot。

## 2. 权限步骤放在最前面

真机操作当天，先配置直连网口、TFTP 和串口 ACL：

```sh
cd /home/nastem/Project/WHUSP
sudo ./scripts/starfive_host_setup.sh up
./scripts/starfive_host_setup.sh status
```

默认宿主机地址是 `192.168.120.1/24`，板子地址是 `192.168.120.230`，TFTP
根目录是 `/tmp/whusp-starfive-tftp`。如果接口、串口或 TFTP 根不同，先通过
`STARFIVE_HOST_IFACE`、`STARFIVE_SERIAL_DEVICE`、`STARFIVE_TFTP_ROOT` 配置
`starfive_host_setup.sh`，后续命令使用同一组参数。

`/tmp` 可能在宿主机重启后清空。决赛前如果要长期保留已准备的大分块，可以把
`STARFIVE_TFTP_ROOT` 统一设为仓库中已忽略的 `tools/starfive_tftp`，host setup、
prepare 和 flash 三处必须使用同一个绝对路径。

准备镜像和离线测试不需要 sudo，也不需要连接开发板或网线。

## 3. 准备新镜像

### 3.1 输入是 `.img`、`.img.gz` 或 `.img.xz`

准备工具会流式解压输入并直接生成 gzip 分块，不会额外落地一份十几 GiB 的
原始镜像：

```sh
python3 scripts/prepare_starfive_sd_image.py \
  /path/to/official.img.gz \
  --output-dir /tmp/whusp-starfive-tftp \
  --prefix final-sd
```

如果安装了 `pigz`，默认自动使用它进行多线程压缩；否则使用 Python 标准库。
可以显式传 `--compressor pigz`。工具默认在每个分块完成后重新解压一次，验证
原始大小、SHA-256 和 CRC32；决赛镜像不要使用 `--skip-verify`。

输出示例：

```text
/tmp/whusp-starfive-tftp/final-sd-manifest.json
/tmp/whusp-starfive-tftp/final-sd-part000.img.gz
/tmp/whusp-starfive-tftp/final-sd-part001.img.gz
...
```

工具拒绝覆盖同名前缀，重新准备另一版镜像时使用新前缀。清单记录输入压缩包
SHA-256、完整原始镜像 SHA-256、每块偏移、原始长度、gzip 长度和两侧校验值。

### 3.2 输入是 `.7z`

7z 只在宿主机端使用。U-Boot 不能直接解压 7z，准备工具会把指定成员流式转换成
独立 gzip 分块：

```sh
7z l /path/to/official.7z
python3 scripts/prepare_starfive_sd_image.py \
  /path/to/official.7z \
  --source-format 7z \
  --archive-member official.img \
  --output-dir /tmp/whusp-starfive-tftp \
  --prefix final-sd
```

必须明确写出镜像成员名，防止多文件归档被错误拼接。

## 4. 网线不在板子旁边时的验证

以下命令会重新计算所有 gzip 分块的大小和 SHA-256，并打印即将执行的精确
`tftpboot`/`gzwrite` 序列，但不会打开串口、配置网络或接触 SD 卡：

```sh
python3 scripts/flash_starfive_sd.py \
  /tmp/whusp-starfive-tftp/final-sd-manifest.json
```

运行离线模拟测试：

```sh
python3 -m unittest \
  scripts.tests.test_prepare_starfive_sd_image \
  scripts.tests.test_flash_starfive_sd \
  scripts.tests.test_build_starfive_sd_maintenance \
  scripts.tests.test_run_starfive_cagent -v
```

模拟器覆盖连续偏移、TFTP 长度、gzip 头、板端 CRC、错误 SD 型号和维护脚本
路径边界。它能验证主机端逻辑，但不能代替最后一次实板 preflight。

维护入口还可以在 QEMU 中实际启动。构建 list FIT 时保留 x1 runner disk：

```sh
python3 scripts/build_starfive_sd_maintenance.py \
  --kernel ./kernel-rv \
  --action list \
  --output /tmp/sd-runs-list-qemu.itb \
  --runner-output /tmp/sd-runs-list-x1.img

make --no-print-directory -C os \
  ARCH=riscv64 MODE=release MEM=4G SMP=4 run-inner \
  KERNEL_ELF="$PWD/kernel-rv" \
  PRIMARY_DISK="$PWD/sdcard-rv-pub.img" \
  AUX_DISK=/tmp/sd-runs-list-x1.img
```

`os/Makefile` 会为 x0/x1 创建临时 qcow2 overlay；list 动作不会删除运行目录，
基础镜像也不会被 QEMU 改写。成功标志是：

```text
STARFIVE_SD_MAINTENANCE_FINAL action=list status=0 count=<数量>
WHUSP_QEMU_OVERLAY state=cleaned
```

## 5. 真机只读 preflight

先复位开发板，让自动脚本截获 `StarFive #`。下面命令严格要求卡名和 U-Boot
显示容量匹配，只把压缩后最小的一块 TFTP 到 RAM 并检查 gzip 头，绝不调用
`gzwrite`：

```sh
python3 scripts/flash_starfive_sd.py \
  /tmp/whusp-starfive-tftp/final-sd-manifest.json \
  --preflight-only \
  --expect-mmc-name SK64G \
  --expect-capacity-gib 59.5
```

`SK64G` 和 `59.5` 是上次实板值，不是所有卡的默认值。换卡后先在 U-Boot
执行只读命令 `mmc dev 1; mmc info`，把实际 `Name:` 和 `Capacity:` 填入参数。
不要因为卡标签写着 64 GB 就跳过 U-Boot 身份核对。

preflight 通过的证据保存在：

```text
tools/starfive_sd_runs/<timestamp>/result.json
tools/starfive_sd_runs/<timestamp>/serial.log
```

## 6. 正式烧写

从 `final-sd-manifest.json` 复制 `image.sha256` 的完整 64 位值。不要使用旧镜像的
SHA-256、CRC 或分块数量。确认供电稳定、网线不会被拔出、目标卡身份正确后执行：

```sh
python3 scripts/flash_starfive_sd.py \
  /tmp/whusp-starfive-tftp/final-sd-manifest.json \
  --execute \
  --confirm-image-sha256 <清单中的完整-image.sha256> \
  --expect-mmc-name SK64G \
  --expect-capacity-gib 59.5 \
  --verify-entry /glibc/cagent_testcode.sh \
  --verify-entry /glibc/buildstorm_testcode.sh \
  --verify-entry /work/tgoskits/Cargo.toml
```

脚本固定执行以下顺序：

1. 重新验证本地每个 gzip 文件；
2. 进入 U-Boot，确认 `gzwrite`、网络和 `mmc 1`；
3. 精确匹配 SD 名称与容量，并确认原始镜像能放入该卡；
4. 每块执行 TFTP、长度检查、gzip 头检查、`gzwrite`；
5. 比较 U-Boot 返回的原始字节数和 CRC32；
6. 全部写完后重新扫描 MMC，并用 `ext4ls` 检查指定路径。

如果新官方镜像的 EXT4 不在 `mmc 1:0`，根据官方布局显式传
`--fs-partition N`。不得仅凭 Linux 的常见分区习惯擅自改成 `1:1`。

### 中断与失败恢复

- 在第一条 `gzwrite` 之前失败：卡没有被本工具修改，修复网络/TFTP 后重试。
- `gzwrite` 已经开始后超时、断电或复位：把卡视为不完整镜像，不要尝试启动。
- 当前工具故意不提供跳块或盲目续传；恢复时使用同一清单，从第 0 块重新烧写。
- 不要在写盘期间按 Ctrl-C。脚本超时后也不会自动向正在工作的 U-Boot 发送
  Ctrl-C。
- 只有所有块 CRC 与最终 `ext4ls` 都通过，才能宣布烧卡成功。

## 7. 清理 BuildStorm 临时运行目录

清理不需要重刷镜像，也不会删除 `/work/tgoskits`。维护 FIT 从 RAM 启动，只允许
处理 `/work/.whusp-buildstorm-runs/run-*` 中经过检查的非符号链接目录。

### 7.1 先生成并运行只列举 FIT

```sh
python3 scripts/build_starfive_sd_maintenance.py \
  --kernel ./kernel-rv \
  --action list \
  --output /tmp/whusp-starfive-tftp/sd-runs-list.itb

python3 scripts/run_starfive_cagent.py \
  --mode sd-maintenance \
  --fit-name sd-runs-list.itb \
  --expect-mmc-name SK64G \
  --expect-capacity-gib 59.5 \
  --reacquire-uboot
```

检查串口日志中的每条 `STARFIVE_SD_MAINTENANCE_ITEM`，确认全是允许删除的历史
`run-*` 目录。list FIT 把 `action=list` 固化在入口脚本中，即使被误启动也不会
进入删除分支。

### 7.2 明确确认后生成并运行删除 FIT

```sh
python3 scripts/build_starfive_sd_maintenance.py \
  --kernel ./kernel-rv \
  --action delete \
  --confirm-delete DELETE-BUILDSTORM-RUNS \
  --output /tmp/whusp-starfive-tftp/sd-runs-delete.itb

python3 scripts/run_starfive_cagent.py \
  --mode sd-maintenance \
  --fit-name sd-runs-delete.itb \
  --expect-mmc-name SK64G \
  --expect-capacity-gib 59.5 \
  --reacquire-uboot
```

成功必须看到且只看到一次：

```text
STARFIVE_SD_MAINTENANCE_FINAL action=delete status=0 count=<数量>
```

遇到根目录是符号链接、匹配项是符号链接/普通文件或删除失败时，维护脚本会输出
`STARFIVE_SD_MAINTENANCE_REFUSE` 并返回非零状态。失败现场不会被扩大清理。

## 8. 收尾

烧写或清理验证完成前保留 manifest、gzip 分块、FIT sidecar 和串口证据。完全结束
后再撤销宿主机临时配置：

```sh
sudo ./scripts/starfive_host_setup.sh down
./scripts/starfive_host_setup.sh status
```

`down` 只停止临时 TFTP 服务、移除直连网口地址和串口 ACL，不删除 TFTP 文件或
证据目录。是否删除宿主机上的大分块应由人明确决定，工具不会自动删除它们。
