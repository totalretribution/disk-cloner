# disk-cloner

Rust CLI that images a whole SSD/disk using [partclone](https://partclone.org/).
Saves the partition table plus a per-partition partclone image (only used blocks,
not free space), compressed. Restores the lot onto a target disk.

It also writes plain disk images (`.iso`, `.img`, or either compressed as
`.zst`) onto a disk, like `zstd -dc x.iso.zst | dd of=/dev/sdc bs=4M conv=fsync`.

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

`restore --shrink` additionally needs `losetup`, `blockdev`, `e2fsck`,
`resize2fs` and `tune2fs` (the `e2fsprogs` and `util-linux` packages).

```
sudo dnf install partclone   # Fedora
sudo apt install partclone   # Debian/Ubuntu
```

## Build

```
cargo build --release
```

## GUI

An optional egui front end, `disk-cloner-gui`, has three screens:

- **Back up**: a drive into a folder.
- **Write**: an image onto a drive. Takes a disk-cloner backup folder or an
  `.iso`, `.img` or `.zst` file and works out which it is, so restoring a backup
  and writing an ISO happen in the same place. Drop either onto the window.
- **Clone**: a drive straight onto another.

```
cargo build --release --features gui
./target/release/disk-cloner-gui
```

The release `.deb` and `.rpm` include it with a desktop entry, so it shows up
in the application menu as Disk Cloner. The release tarball carries
`disk-cloner.desktop` and `disk-cloner.svg` to install by hand.

Run it as your normal user. Drives stay listed in a sidebar, with the running
system's drive locked as a target. Each screen shows its plan before anything
runs, read directly without root: the partitions and how each is copied, the
refitted layout when a backup goes onto a smaller drive (the same projection
`restore --shrink --dry-run` prints), and the checksum an image will be checked
against.

The button at the bottom runs the `disk-cloner` CLI through `pkexec`, so only
that process runs as root and you get your desktop's password dialog. The CLI
must sit next to `disk-cloner-gui` or be on `PATH`.

- The CLI's "retype the target path" check appears as a dialog listing what is
  on the drive. Cancel it and the CLI aborts without writing.
- The progress screen shows each stage as the CLI reaches it, partclone's or
  `write`'s live progress, and the raw output. **Stop** sends SIGTERM to the
  whole job, which leaves a half-written target unusable.

The GUI embeds IBM Plex Sans and Mono (SIL Open Font License, see
`assets/fonts/OFL.txt`).

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

# write an ISO / raw image to a USB stick (prints plan unless --yes; ERASES target)
sudo disk-cloner write kiosk-base-v0.1.10.iso.zst /dev/sdc --yes

# restore onto a SMALLER disk: see the projected layout, write nothing
sudo disk-cloner restore /mnt/backup/laptop /dev/sdb --shrink --dry-run

# ...then do it
sudo disk-cloner restore /mnt/backup/laptop /dev/sdb --shrink --yes
```

## Writing ISO and raw images

`write` streams an image straight onto a whole disk. The format is detected from
the file's first bytes, not its name:

- **raw** — `.iso`, `.img`, anything without a recognised compression header
- **zstd** — decompressed on the fly (built in; the `zstd` tool is not needed).
  Includes `pzstd` output and images made with `--long` up to `--long=31`.

gzip and xz images are refused rather than written compressed. Decompress them
first or recompress with zstd.

Before writing it checks that the target is a whole disk, is not mounted, and is
big enough. For zstd the exact uncompressed size is read from the frame headers
without decompressing (milliseconds, even for multi-frame and `pzstd` files). If
any frame does not record its size, as with `cat x.iso | zstd`, the image is
decompressed once without writing anything to measure it, so a too-small target
is refused before it is touched. The device is opened exclusively, so the kernel refuses the
write if anything still holds it. Data is flushed every 64 MiB, so the progress
line shows what has reached the disk, and the device is fsynced before the tool
reports success. `partprobe` then re-reads the new partition table.

### Checksums

If `<image>.sha256` sits next to the image (e.g. `kiosk.iso.zst.sha256`), it is
checked automatically. Point at another file, or give a hash directly, with
`--sha256`; skip the check with `--no-checksum`.

```
sudo disk-cloner write kiosk.iso.zst /dev/sdc --sha256 SHA256SUMS --yes
sudo disk-cloner write kiosk.iso.zst /dev/sdc --sha256 8f4343...7aa4 --yes
```

Accepted: `sha256sum` output (`hash  name` or `hash *name`), BSD style
(`SHA256 (name) = hash`) or a bare hash. A file listing several images is
matched by file name.

- A hash of the **image file** (the entry names `kiosk.iso.zst`, or a bare hash)
  is checked before anything is written, so a corrupt download never reaches
  the disk.
- A hash of the **decompressed image** (the entry names `kiosk.iso`) is checked
  by decompressing once before writing, then checked again against the data
  written.

A checksum catches corrupt or incomplete downloads. It does not prove the image
is genuine if the hash came from the same place as the image.

## Restoring onto a smaller disk

A plain restore replays the saved partition table sector for sector, so the
target must be at least as big as the source. `--shrink` refits the layout
instead.

For each ext2/3/4 partition it restores the image into a sparse file on a loop
device, measures the true floor with `resize2fs -P`, shrinks the filesystem, and
copies only the used blocks onto the new, smaller partition. Swap is recreated
by `mkswap` at whatever size it is given. Everything else keeps its original
size, because partclone cannot resize it — so the refit fails outright if those
partitions alone do not fit.

It works in the other direction too: on a larger target the ext filesystems grow
to fill the extra space.

```
sudo disk-cloner restore ./laptop /dev/sdb --shrink --dry-run
```

```
=== Projected refit (estimated from image headers) ===
Scratch  : /var/tmp — need 14.3 GiB, 422.0 GiB free   OK
Image    : ./laptop
  created 2026-06-29T13:01:53Z, source /dev/sda (465.8 GiB)
Target   : /dev/sdb (111.8 GiB) — WILL BE ERASED

  #  fs        original         new   how
  1  vfat       976.0 MiB   976.0 MiB   unchanged (vfat cannot resize)
  2  ext4       440.8 GiB   104.6 GiB   resized (12.9 GiB in use)
  3  swap        24.0 GiB     6.2 GiB   recreated by mkswap
                             111.8 GiB  of 111.8 GiB usable
```

`--dry-run` reads only the image headers, so it is instant and touches nothing.
Drop it and the tool stages the filesystems, prints the same table again with the
measured sizes and the real start/sector figures, and only then asks you to
retype the target path.

### Scratch space

Staging needs room for the *used* data of every ext partition at once — not the
filesystem size. The figure comes from the partclone image header (`used_blocks
× block_size`), so the check is exact rather than a guess, and the run aborts
before staging if the space is not there. Point `--scratch` at a filesystem with
enough room; it defaults to `/var/tmp`.

### Two or more large ext partitions

Each gets its own measured minimum first, then whatever is left over is divided
between them in proportion to their original sizes — so one does not end up at
its floor while another keeps everything. Peak scratch use is the sum of their
used data, since all of them are staged before the target is written.

Override the share-out per partition with `--part-size`:

```
# pin partition 2 at 40 GiB, let 3 take the remainder, cap swap at 2 GiB
sudo disk-cloner restore ./laptop /dev/sdb --shrink \
    --part-size 2=40G --swap-size 2G --dry-run
```

### Flags

| Flag             | Effect                                                        |
|------------------|---------------------------------------------------------------|
| `--shrink`       | Refit the layout onto a differently sized disk                 |
| `--dry-run`      | Print the projected layout and stop (with `--shrink`)          |
| `--part-size N=SIZE` | Pin partition N to an exact size; repeatable               |
| `--swap-size SIZE`   | Size every swap partition; `0` drops swap entirely         |
| `--scratch DIR`  | Where to stage filesystems (default `/var/tmp`)                 |
| `--keep-scratch` | Leave staging files and loop devices in place for inspection    |

### Notes

- The **source disk must not be mounted** (offline image). Boot from live media
  or unmount first. `--force` overrides the check but risks an inconsistent image.
- `restore --yes` still prompts you to retype the target path before erasing.
- Restore rewrites the partition table by default; `--skip-ptable` restores into
  existing partitions.
- Restoring onto a smaller disk needs `--shrink`; without it the tool refuses up
  front rather than failing part-way through `sfdisk`.
- `--shrink` can only shrink ext2/3/4. vfat and xfs cannot shrink at all, so
  those partitions keep their original size.
- Partition UUIDs, type GUIDs and names are carried over unchanged, and swap
  keeps its UUID/label, so `fstab` and bootloader entries keep resolving.

## Commands

| Command   | Purpose                                            |
|-----------|----------------------------------------------------|
| `list`    | Show block devices                                 |
| `backup`  | Image a disk → directory (`-c zstd\|gzip\|none`)     |
| `restore` | Write an image directory → target disk             |
| `write`   | Write a raw `.iso`/`.img` (optionally `.zst`) → disk |
| `info`    | Print an image's `manifest.json`                   |
