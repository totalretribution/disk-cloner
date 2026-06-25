# disk-cloner

Rust CLI that images a whole SSD/disk using [partclone](https://partclone.org/).
Saves the partition table plus a per-partition partclone image (only used blocks,
not free space), compressed. Restores the lot onto a target disk.

## How it works

- **partition table** — dumped with `sfdisk -d`, restored with `sfdisk`.
- **each partition** — imaged with the matching `partclone.<fs>` binary
  (`ext4`, `xfs`, `btrfs`, `ntfs`, `vfat`, `exfat`, `f2fs`, `hfsplus`), falling
  back to `partclone.dd` (raw) for swap/unknown/unformatted. partclone only
  copies used blocks, so images are far smaller than the raw disk.
- **compression** — piped through `zstd` (default), `gzip`, or `none`.
- **manifest.json** — records device, sizes, fstypes, cloner used, filenames.

## Requirements

Runtime tools on `PATH`: `lsblk`, `sfdisk`, `partprobe`, `partclone.*`,
and `zstd`/`gzip` if compressing. Root required for backup/restore (raw block
access).

```
sudo dnf install partclone   # Fedora
sudo apt install partclone   # Debian/Ubuntu
```

## Build

```
cargo build --release
```

## Usage

```
# see disks/partitions
disk-cloner list

# preview the plan (no root, writes nothing)
disk-cloner backup /dev/nvme0n1 -o /mnt/backup/laptop --dry-run

# image the disk
sudo disk-cloner backup /dev/nvme0n1 -o /mnt/backup/laptop

# inspect an image
disk-cloner info /mnt/backup/laptop

# restore onto another disk (prints plan unless --yes; ERASES target)
sudo disk-cloner restore /mnt/backup/laptop /dev/sdb --yes
```

### Notes

- The **source disk must not be mounted** (offline image). Boot from live media
  or unmount first. `--force` overrides the check but risks an inconsistent image.
- `restore --yes` still prompts you to retype the target path before erasing.
- Restore rewrites the partition table by default; `--skip-ptable` restores into
  existing partitions.

## Commands

| Command   | Purpose                                            |
|-----------|----------------------------------------------------|
| `list`    | Show block devices                                 |
| `backup`  | Image a disk → directory (`-c zstd\|gzip\|none`)     |
| `restore` | Write an image directory → target disk             |
| `info`    | Print an image's `manifest.json`                   |
