#!/usr/bin/env python3
"""storefull.py: a store on a disk that has no room.

The store is on a small file system of its own (a tmpfs, mounted with sudo), and a file next
to it takes up all but so many bytes, so that the daemon runs out of room at another place
each time: before it can say which format the store is of, when it opens its pack, in the
middle of the records, and when it seals. Each time:

  - the daemon has to build a program that prints what cargo's does, on its first build and
    after an edit;
  - it has to say that it keeps nothing, and why;
  - `rustd cache verify` has to find nothing in the store that is not what was written.

Then room is made under a running daemon, and what the next daemon compiles has to be kept
again. `rustd cache pull` into the full store has to fail with the reason and leave no part
of a pack behind, and `rustd cache gc` has to make room.

    python3 storefull.py
"""
import os, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from storelib import Checkout, Daemon, cache, files, scratch, stats, verify

SIZE = 8 << 20
# How much room the store has, in bytes. A cold build of the workspace writes about 1.5 MB.
ROOMS = [0, 4096, 16 << 10, 128 << 10, 512 << 10, 1 << 20]
SEALS = {"RUSTD_CACHE_SEAL_AT": "1", "RUSTD_CACHE_MAX": "off"}
NO_ROOM = "No space left on device"


def fail(what):
    raise SystemExit(f"FAIL: {what}")


class Disk:
    """A file system of SIZE bytes with the store in it."""

    def __init__(self, work):
        self.at = os.path.join(work, "disk")
        os.mkdir(self.at)
        options = f"size={SIZE},uid={os.getuid()},gid={os.getgid()},mode=700"
        mounted = subprocess.run(["sudo", "-n", "mount", "-t", "tmpfs", "-o", options, "tmpfs", self.at],
                                 capture_output=True, text=True)
        if mounted.returncode != 0:
            fail(f"a small file system could not be mounted (sudo is needed): {mounted.stderr.strip()}")
        self.store, self.ballast = os.path.join(self.at, "store"), os.path.join(self.at, "ballast")

    def free(self):
        found = os.statvfs(self.at)
        return found.f_bavail * found.f_frsize

    def leave(self, room):
        """Takes up all of the disk but `room` bytes, to the block."""
        if os.path.exists(self.ballast):
            os.remove(self.ballast)
        take = self.free() - room
        if take < 0:
            fail(f"the disk has {self.free()} bytes free, fewer than {room}")
        with open(self.ballast, "wb") as ballast:
            ballast.write(bytes(take))
        return self.free()

    def unmount(self):
        subprocess.run(["sudo", "-n", "umount", self.at], capture_output=True)


def built_without(checkout, disk, room):
    """A daemon on a store with `room` bytes builds, and says what it could not keep."""
    shutil.rmtree(disk.store, ignore_errors=True)
    os.mkdir(disk.store)
    left = disk.leave(room)
    checkout.set(room % 7)
    daemon = Daemon(checkout, disk.store, name=f"room{room}", env=SEALS)
    checkout.check(daemon, f"{room} bytes of room, the first build")
    checkout.set(room % 7 + 1)
    checkout.check(daemon, f"{room} bytes of room, after an edit")
    log = daemon.log()
    daemon.kill()
    said = [line for line in log.splitlines() if line.startswith("store:") and NO_ROOM in line]
    if not said:
        fail(f"{room} bytes of room: the daemon does not say the disk is full: {daemon.log_path}")
    if "panicked" in log:
        fail(f"{room} bytes of room: the daemon panicked: {daemon.log_path}")
    whole, numbers, out = verify(disk.store)
    if not whole:
        fail(f"{room} bytes of room: the store does not verify\n{out}")
    if files(disk.store, ".tmp"):
        fail(f"{room} bytes of room: a part of a file was left: {files(disk.store, '.tmp')}")
    not_used = any("store: not used" in line for line in said)
    print(f"  {left:>8} bytes free: built; {'store not used' if not_used else 'nothing more kept'}; "
          f"{numbers.get('packs', 0)} packs, {numbers.get('open packs', 0)} open in the store")
    return not_used


def kept_again(checkout, disk):
    """Room is made while a daemon runs; the next build that writes the store keeps what it compiles."""
    shutil.rmtree(disk.store, ignore_errors=True)
    os.mkdir(disk.store)
    disk.leave(128 << 10)
    checkout.set(11)
    daemon = Daemon(checkout, disk.store, name="again", env=SEALS)
    checkout.check(daemon, "before room is made")
    if NO_ROOM not in daemon.log():
        fail(f"the disk was not full before room was made: {daemon.log_path}")
    os.remove(disk.ballast)
    daemon.kill()
    daemon = Daemon(checkout, disk.store, name="again", env=SEALS)
    checkout.check(daemon, "after room was made")
    log = daemon.log()
    daemon.kill()
    whole, numbers, out = verify(disk.store)
    if NO_ROOM in log or not whole or numbers["records"] < 1000:
        fail(f"with room again the daemon did not keep what it compiled: {daemon.log_path}\n{out}")
    daemon = Daemon(checkout, disk.store, name="again", env=SEALS)
    checkout.check(daemon, "from the store that was full")
    log = daemon.log()
    daemon.kill()
    if "store: 0 functions found" in log or "functions found" not in log:
        fail(f"nothing was found in the store that was full: {daemon.log_path}")
    print(f"  room made: {numbers['packs']} packs with {numbers['records']} records kept, and found by the next daemon")


def commands(checkout, disk, work):
    """`rustd cache` on a full disk: pull fails cleanly, gc makes room."""
    remote = os.path.join(work, "remote")
    status, out = cache(disk.store, "push", remote)
    if status != 0:
        fail(f"push: {out}")
    packs = len(files(disk.store, ".pack"))
    status, out = cache(disk.store, "gc", "--max", "0")
    if status != 0 or files(disk.store, ".pack"):
        fail(f"gc --max 0: {out}")
    disk.leave(64 << 10)
    status, out = cache(disk.store, "pull", remote)
    if status == 0 or NO_ROOM not in out:
        fail(f"pull into a full store did not fail with the reason: {out}")
    whole, _, verified = verify(disk.store)
    if not whole or files(disk.store, ".tmp"):
        fail(f"pull into a full store left something behind: {os.listdir(disk.store)}\n{verified}")
    os.remove(disk.ballast)
    status, out = cache(disk.store, "pull", remote)
    whole, numbers, verified = verify(disk.store)
    if status != 0 or not whole or numbers["packs"] != packs:
        fail(f"pull with room: {out}\n{verified}")
    # A full disk with a store on it: collecting has to work with no room at all.
    disk.leave(0)
    status, out = cache(disk.store, "gc", "--max", "100K")
    after = stats(disk.store)
    if status != 0 or after["pack bytes"] > 100 << 10:
        fail(f"gc on a full disk: {out}")
    print(f"  commands: pull into a full store refused ({NO_ROOM}), nothing left behind; "
          f"pull with room fetched {packs} packs; gc on a full disk left {after['pack bytes']} bytes")


def main():
    start = time.time()
    work = scratch("rustd-storefull-")
    checkout = Checkout(work)
    disk = Disk(work)
    try:
        not_used = sum(built_without(checkout, disk, room) for room in ROOMS)
        kept_again(checkout, disk)
        commands(checkout, disk, work)
    finally:
        disk.unmount()
    shutil.rmtree(work, ignore_errors=True)
    print(f"PASS: {len(ROOMS)} full disks built without a store or without the rest of it "
          f"({not_used} could not open it at all), in {time.time() - start:.0f}s")


if __name__ == "__main__":
    main()
