# Remote hydration over S3

Branch `rustd-s3`, from `rustd-collate` at `27315323cb`.

`rustd cache push REMOTE` and `rustd cache pull REMOTE` had an S3 path that had never been
run. This is what it took to make it real, what was measured, and what is still open.

**Real AWS S3 was not run.** There are no AWS credentials in the session this was done in.
Everything below was measured against MinIO (`RELEASE.2026-09-22T19-25-18Z`) and SeaweedFS
(4.48) on the same machine, over loopback. The harness runs against a real bucket with one
variable, `RUSTD_S3_REMOTE=s3://bucket/prefix`; nobody has done that yet.

## What changed

The S3 client is now the AWS SDK for Rust (`aws-config`, `aws-sdk-s3`) in place of
`rusty-s3` and `ureq`. The reason is credentials: `rusty-s3` signs with a key pair you hand
it, and the task asks for the sources the AWS tools read. With the SDK's default chain rustd
takes credentials from the environment, the shared config and credentials files (and a
named profile), web identity tokens, ECS container credentials and EC2 instance roles,
without rustd having any code for them. Of those, the environment and the shared files with a profile
were run; web identity, container and instance roles were not, because there is nothing here
that issues them.

The code is in `crates/rustd/src/remote.rs` and three files under `crates/rustd/src/remote/`:

- `s3.rs`: the S3 calls. Ranged GET, HEAD, conditional PUT for lists, multipart upload.
- `transfer.rs`: fetching an object into a file. Resuming, hashing, the limit on requests
  in flight, the counters.
- `signed.rs`: signed manifests.

`crates/rustd/src/deps.rs` (`rustd deps push|pull`, cargo's compiled units) moved onto the
same remote, so it has the same verification, retries and counters.

The remote is async on one tokio runtime of two worker threads, built once in a process.
The daemon's own threads are synchronous and wait on that runtime from outside; nothing
calls `block_on` from inside a task.

### Layout of a remote

    packs/<sha256 of the pack>.pack
    keys/<sha256 of the key list>          the 32-byte keys of one pack, back to back
    manifests/<project>                    one line a pack: PACK SIZE KEYS
    units/<sha256>, deps/<project>         cargo's units, the same way

### Address and configuration

    s3://bucket/prefix?endpoint=URL&region=NAME&profile=NAME&path-style=true|false

Anything the address leaves out comes from where the AWS tools look: `AWS_ENDPOINT_URL`,
`AWS_REGION`, `AWS_PROFILE`, `~/.aws/config`. An unknown option is refused by name. With an
endpoint of its own rustd addresses the bucket by path unless told `path-style=false`,
because that is what most services that are not AWS need; without one it does what the SDK
does for AWS.

| Variable | Meaning | Default |
| --- | --- | --- |
| `RUSTD_REMOTE` | the remote the daemon pulls from before a build and pushes to after | none |
| `RUSTD_PROJECT` | which manifest | `default` |
| `RUSTD_REMOTE_JOBS` | requests in flight at once | 8 |
| `RUSTD_REMOTE_TIMEOUT` | seconds a request may go without an answer, or a body without a byte | 30 |
| `RUSTD_REMOTE_READONLY` | never write to the remote | off |
| `RUSTD_REMOTE_LAZY` | fetch a pack when a build first asks for a key in it | off |
| `RUSTD_REMOTE_SIGN` | file with the key this machine signs manifests with | none |
| `RUSTD_REMOTE_TRUST` | public keys this machine takes manifests from | none |

In read-only mode `cache push` and `deps push` fail with "RUSTD_REMOTE_READONLY is set:
nothing is sent", and the daemon does not push.

Credentials are never printed. rustd does not hold them; the SDK does. Errors are printed
as the service's error code and message, or the HTTP status, or the chain of causes of a
transport error, none of which contain the request's headers. The harness greps everything
rustd printed in the whole run for the secret key (and the session token when run against a
real bucket), forwards and reversed.

## Robustness

**Fetch.** An object is written to `<name>.partial` under an exclusive lock, never to its
final name. When the body breaks off, rustd asks again with `Range: bytes=N-` from the byte
it stopped at, up to 6 times in a row without getting further, waiting 250 ms, 500 ms, ...
8 s in between; getting any bytes starts the count again. When the object is whole, its
SHA-256 is compared with the name it was listed under, the file is synced, and only then
renamed to `<hash>.pack`. So a pack file in the store is always a whole, verified pack: a
killed rustd, a killed server or a full disk leaves a `.partial` or nothing. The next
process to run picks the `.partial` up and fetches only the rest. If what is in the
`.partial` turns out not to be the start of this object (the hash does not match at the
end), it is thrown away and fetched once from the start before the object is rejected.

**Send.** A file of 16 MiB or more goes up as a multipart upload in 8 MiB parts, with at
most `RUSTD_REMOTE_JOBS` requests in flight across everything the process is doing. A part
that fails is sent again by the SDK (three attempts, exponential backoff with jitter). If
the upload fails it is aborted, and a push first aborts uploads an earlier process left
unfinished under the same key. The manifest is written last, after every pack and key list
it names is in the bucket, with `If-Match`/`If-None-Match` so that two machines pushing at
once do not undo each other (up to 8 rounds of read, extend, write).

**Timeouts.** Connecting: 5 s at most. An attempt with no answer, or a read with no bytes:
`RUSTD_REMOTE_TIMEOUT`. An upload attempt gets that plus a second for every 64 KiB, so a
slow link is not mistaken for a dead one.

**Failures are counted.** Every command ends with a line like

    remote: 15 requests (0 tried again, 0 transfers resumed), 0 bytes received, 42336756 sent, 0 objects rejected

and a push or pull in which anything failed prints each failure and exits non-zero with
"N failed, M of them rejected". In the daemon a remote that fails is not an error to the
build: it says so and compiles what it would have fetched.

## Trust

There are two different questions and they have different answers.

**Can the storage change the code?** No. A pack is named by the SHA-256 of its bytes and
is hashed after the fetch and before the rename, so a bucket (or a proxy, or a bad disk on
the way) that returns other bytes for a name gets them thrown away, counted under "objects
rejected", and reported: "rejected packs/X.pack: its N bytes hash to Y, not to X". A machine
also hashes each of its own packs before sending it and refuses to send one that a bad disk
spoiled.
Key lists and cargo units are named and checked the same way. The store checks a CRC32 of
each record again when it reads it. After a rejection the build compiles the functions
itself, so the result is the code a local build gives.

**Can the storage make rustd use code for the wrong thing?** A cache entry's key is a
SHA-256 over the rustd executable's own hash, the backend and optimisation level, the target
triple, and the function's stable name. The stable name holds the function's path, its
generic arguments spelled out, and for the crate it is in either rustc's stable crate id
(dependencies: name, version and the metadata cargo passes) or a hash of the crate's source
(crates of the checkout). So an entry is only ever looked up by a machine running the same
rustd binary, for the same backend and target, for the same instance of the same crate
source. A remote cannot move an entry to another key without changing the pack's bytes and
so its name.

**What is left is the manifest**, which is the one mutable object and says which packs to
fetch. Somebody who can write to the bucket can upload a well-formed pack of their own
making, with wrong code under real keys, and list it; its name is honestly the hash of its
bytes. Content addressing cannot catch that. Signing does: with `RUSTD_REMOTE_SIGN` a
machine signs each manifest it writes (Ed25519, over the manifest's object name and its
body, so one project's manifest cannot be replayed as another's), and with
`RUSTD_REMOTE_TRUST` a machine refuses any manifest that is not signed by a key it lists.
`rustd cache keygen FILE` makes a key and prints the public half.

So the honest statement is: with `RUSTD_REMOTE_TRUST` set, nobody without a trusted signing
key can make rustd emit different code than a local build, whatever they do to the bucket.
Without it, whoever can write to the bucket is trusted, the same as with sccache or a cargo
registry mirror. A trusted signer is trusted completely; rustd does not recompile to check
them. One more assumption sits under all of it and predates this branch: two machines with
the same rustd binary compile the same key to equivalent code. That is what the local store
already relies on between workspaces.

Tested by the harness, on both servers:

- pack truncated, extended, emptied, one byte flipped: rejected, reported, no pack in the store;
- a different valid pack stored under A's pack name: rejected, B compiles its own, and its
  program prints what cargo's build prints;
- a pack the manifest lists that is gone: reported as "is listed and is not there", exit non-zero;
- with trust on: a manifest forged under A's old signature, one with no signature, and one
  signed by a key B does not list are each refused, nothing is fetched, and the program is
  the same;
- a machine that trusts keys but has none to sign with refuses to push.

## Lazy hydration

With `RUSTD_REMOTE_LAZY=1` the daemon fetches only the key lists (32 bytes a function)
before a build. When the store is asked for a key it lacks and a listed pack holds it, the
compiling thread fetches that pack, verified as above, and looks again. A pack nobody asks
of is never fetched; the harness checks that.

It did not win in what was measured (below): 8.7 s against 8.6 s for ripgrep, 82.9 s
against 80.9 s for cargo. A fresh machine building the same workspace wants every pack, so
lazy fetches the same bytes plus the key lists, and fetches packs one after another as
threads trip over them instead of eight at a time. Where it should win is a manifest that
holds much more than one build needs, such as one project name shared by many workspaces or
a long history of pushes. That is an inference; the only evidence is the harness case where
an unused pack is not fetched.

## Measurements

Machine: 8 cores, 31 GB, Linux x86-64. Cranelift backend. MinIO on loopback behind the
harness's counting proxy, so transfer times are a lower bound; they say nothing about a real
network. Each "machine" is its own HOME, checkout, cargo target directory and rustd store
in a fresh directory. They are not separate containers. One run each, no repeats, so read
differences of a few percent as noise.

"To a running program" is everything a machine with a checkout and a toolchain does: pull
cargo's units if any, `cargo rustc` for the dependencies and link line, rustd's build, run
the binary. Crates.io sources were already in `~/.cargo` for all of them.

    cd harness
    RUSTD_BIN=.../rustd python3 remote.py --measure ~/work/rg-a -p ripgrep --bin rg \
        --crate rg,grep,grep_cli,grep_matcher,grep_printer,grep_regex,grep_searcher,globset,ignore
    RUSTD_BIN=.../rustd python3 remote.py --measure ~/work/cargo-a -p cargo --bin cargo \
        --crate cargo,cargo_util,cargo_platform,cargo_util_schemas,crates_io,rustfix,cargo_credential

ripgrep 14.1.1 (9 crates by rustd; 22,340 functions compiled cold):

| Fresh machine B | To a running program | cargo | rustd | Functions compiled | Requests | Bytes down |
| --- | --- | --- | --- | --- | --- | --- |
| no remote | 15.2 s | 4.6 s | 10.6 s | 22,340 | 0 | 0 |
| rustd's packs | 13.1 s | 4.6 s | 8.4 s | 0 | 2 | 41,551,604 |
| packs and cargo's units | 8.6 s | 2.0 s | 6.3 s | 0 | 51 | 100,302,333 |
| the same, lazy | 8.7 s | 1.9 s | 6.5 s | 0 | 52 | 101,087,485 |

Machine A's push: 1 pack, 41,551,465 bytes in 316 ms, 15 requests (a multipart upload);
48 cargo units, 58,746,346 bytes in 437 ms, 99 requests. Pulling the pack took 157 ms.

cargo 0.92.0 (7 crates by rustd; 226,180 functions compiled cold, 3,599 of 3,602 functions
of the workspace; the other three are refused by rustd and compiled by rustc, as on the base branch):

| Fresh machine B | To a running program | cargo | rustd | Functions compiled | Requests | Bytes down |
| --- | --- | --- | --- | --- | --- | --- |
| no remote | 132.7 s | 42.9 s | 89.7 s | 226,180 | 0 | 0 |
| rustd's packs | 105.7 s | 42.7 s | 63.0 s | 0 | 15 | 928,619,320 |
| packs and cargo's units | 80.9 s | 25.0 s | 54.0 s | 0 | 449 | 1,429,301,619 |
| the same, lazy | 82.9 s | 25.2 s | 55.9 s | 0 | 463 | 1,436,478,419 |

Machine A's push: 14 packs, 928,617,374 bytes in 3.2 s, 211 requests; 433 units,
500,642,176 bytes in 3.6 s, 869 requests. The bucket ends with 463 objects, 1,436,478,419
bytes. Pulling the 14 packs took 2.7 to 3.3 s.

In every row the program printed the same as the one built without the remote.

What the numbers say: the remote removes all of rustd's code generation (0 functions
compiled) and that is worth 2 s of 15 for ripgrep and 27 s of 133 for cargo. With cargo's
units as well it is 15.2 s to 8.6 s and 132.7 s to 80.9 s. What is left is not transfer,
which is 0.4 s and under 5 s. It is rustd loading the workspace and walking every instance
to find its key (the "cold:" line still reads 3.1 s for ripgrep and 44 s for cargo with
nothing to compile), and cargo running build scripts and proc macros. Those are the next
thing to cut and are not a remote problem.

The store is large for what it holds: 929 MB of packs for cargo. Packs are not compressed
on the wire or at rest. On a real network at, say, 50 MB/s that is 19 s of the 52 s saved,
so compression would matter there. That sentence is arithmetic, not a measurement.

## The harness

`harness/remote.py` with `harness/s3fake.py`. It starts MinIO and SeaweedFS (found through
`RUSTD_MINIO`, `RUSTD_WEED`, PATH or `~/s3tools`), each behind a proxy of its own that
counts requests and bytes and can answer with a status, reset the connection, cut a response
or a request body at a byte, stall, or throttle. It builds a small generated workspace on
machine A, and then checks, 111 checks in all across the two servers:

- the round trip: A compiles and pushes, B with another HOME and an empty store pulls,
  compiles 0 functions, and prints what cargo's build prints; a second push sends nothing;
  every object in the bucket hashes to its name;
- configuration: read-only, a profile in the shared files, `AWS_PROFILE`,
  `AWS_ENDPOINT_URL`, region and path style, the bucket in the host name (MinIO, through
  `bucket.localhost`), a wrong secret, an unknown option, no secret in any output;
- multipart with bounded concurrency, and rustd's request count equal to the proxy's;
- 503 twice, 503 on a part, on completion and on the manifest, 503 forever;
- connection dropped mid-body twice, closed with no answer twice, dropped mid-part;
- a stall longer than the timeout, an answer slower than the timeout;
- rustd killed mid-fetch, the next run fetching only the rest, a stale `.partial` of
  something else;
- two processes fetching into one store; twelve packs with three requests at a time;
- the server killed mid-fetch and restarted, killed and left dead, killed mid-push, and
  each of those recovered afterwards with no unfinished upload left in the bucket;
- the poisoning and signing cases listed under Trust;
- lazy: nothing compiled, same output, the unused pack not fetched.

    cd harness && RUSTD_BIN=../target/release/rustd python3 remote.py

Takes about five minutes. With the code and the harness as committed it was run three times in a row and gave
111/111 each time (5m27s, 5m23s, 5m22s). Earlier runs, while it was being written, gave 104, 110, 110,
111, 110, 111, 111, 111 and 110; each miss was looked into and is either fixed or described
here.

One check is conditional. MinIO reads again 1.3 s after a kill and a fetch always rides
that out. SeaweedFS took between 14 and 23 s to read again in these runs, on either side of
the 16 s rustd waits for a byte. So the harness times how long the server was away: under
10 s the fetch has to succeed with at least one resume; over that it may either succeed, or
fail with no pack in the store and succeed when run again. Before that was written down the
check failed on SeaweedFS in two runs of eight.

Against a real bucket: `RUSTD_S3_REMOTE=s3://bucket/prefix python3 remote.py`, with
credentials wherever the AWS tools would find them. That runs the round trip, the
configuration, poisoning, signing and lazy checks, and skips the ones that need the proxy
or need to kill the server. **Not run.**

## Regression gate

Run on Linux x86-64 with the release binaries of this branch, under
`RUSTD_BACKEND=cranelift` and under `RUSTD_BACKEND=llvm RUSTD_OPT=2 RUSTD_EMIT=fast`
(LLVM 20.1). Both gave the same counts:

| Script | Result |
| --- | --- |
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 803/803 functions from rustd, 0 of 892 layouts differ |
| `programs.py` | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts |
| `breakers.py` | 25 of 25, 0 open, 0 functions skipped |
| `unwinds.py` | 8 of 8 |
| `splices.py` | 71 of 71 |
| `asmedits.py` | 10 of 10 |
| `includes.py` | 9 of 9 |
| `worktrees.py` | 52 of 52 |

`cargo fmt --all -- --check` is clean. `cargo test --release -p hir-ty -p hir-def -p parser
-p syntax -p rustd`: 511, 4, 330, 14 and 56 passed, 0 failed (rustd's 14 include the
signing tests).

## What is still open

- **Real AWS has not been run.** The SDK is the same code against AWS as against MinIO, but
  virtual-hosted addressing against real DNS, IAM denial messages, and role credentials
  (web identity, ECS, EC2) are untested. Needs credentials.
- **The machines were directories, not containers,** and the network was loopback. Transfer
  times are best cases.
- **Packs are not compressed.** See above; 929 MB for cargo.
- **Nothing is ever deleted from a remote.** Manifests only grow. There is no garbage
  collection and no expiry, and lazy hydration is the only thing that keeps a long manifest
  from costing every fresh machine its whole history.
- **Lazy fetches block a compiling thread each** and are not batched; it did not win here.
- **Without `RUSTD_REMOTE_TRUST`, a writer to the bucket is trusted.** That is a choice of
  default, not a bug, but it should be decided before this is open-sourced whether an
  unsigned remote needs an explicit opt-in.
- **The lazy hook is one per process** (`fetch_missing_with` in `store.rs`, first call
  wins). That fits a daemon with one remote; it would have to become part of the store if a
  daemon ever serves two.
- **Small objects get fewer tries than packs.** A pack fetch resumes for about 16 s
  without a byte before it gives up. A manifest or key list is one request, and gets the
  SDK's three attempts, a second or two. SeaweedFS, just restarted after a kill, lists its
  bucket and then answers reads with 500 for many seconds more; a pull in that window fails
  on the manifest with the service's error and has to be run again. The harness hit this
  once and now waits until the service can read before it goes on to the next check. Giving
  lists the same backoff as packs needs rustd to tell a refusal (403) from an outage itself;
  not done.
- **One failed pack stops the rest.** When a fetch runs out of resumes the remote is taken
  to be down and the other transfers of that process stop rather than each waiting out its
  own 16 s. The next run resumes them, and the daemon's push after a build tries afresh.
- **The daemon talks to the remote twice:** a pull when it starts and a push after its
  first build. A daemon that stays up for a day does not see what others push meanwhile
  and does not push what later edits made it compile. `rustd cache push|pull` can be run
  beside it.
- **Windows and macOS were not run.** The partial-file lock and the rename are std calls
  that exist there; the harness is Linux only.
