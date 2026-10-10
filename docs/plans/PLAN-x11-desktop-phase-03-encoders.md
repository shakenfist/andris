# Phase 3: Image encoders (ryll)

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: high. The master plan asked for medium, on the
grounds that encoders follow an established pattern. The survey
changed that. Before anything can be encoded, ryll's LZ4 decoder has
to be rewritten to match spice-server (ryll#475). That is a protocol
question about a shared crate that ryll's client runs, which this
project's template plans at high effort.

## Scope

This plan lives in andris, beside its master plan. The code lands in
ryll as one pull request on a ryll branch named
`x11-desktop-phase-03-encoders`, which links back here, as phase 2 did.
This andris branch carries only the phase 2 close-out and this plan.

In:

- **A real LZ4 capture.** Capture LZ4 images sent by spice-server and
  settle their framing against them. This needs a ryll option to ask
  for LZ4, since ryll never does today.
- **A rewritten LZ4 decoder** that matches spice-server and
  spice-common. Ryll's renderer and protocol crate change to match.
  Fixes ryll#475.
- **LZ4 and JPEG image encoders** in `shakenfist-spice-compression`,
  behind a non-default `encode` feature.
- **Tests:**
  - round-trip and property tests for both encoders;
  - fixture tests for the decoder, against the captured bytes and
    against streams that liblz4 itself produced;
  - a fuzz target for the new decoder.
- Ryll documentation for all of the above.

Out:

- **JPEG_ALPHA.** It carries its alpha plane as a SPICE-LZ stream, so
  it needs an LZ encoder. Andris sends no alpha: an X root window is
  opaque.
- **QUIC, LZ and GLZ encoders.** The master plan settles on LZ4 and
  JPEG.
- **Image caching.** The descriptor's `CACHE_ME`, and tracking the
  client's pixmap cache, belong to whichever andris phase first needs
  them.
- **Choosing between LZ4 and JPEG.** That heuristic is phase 5's.
- **A ryll release.** See decision 8. Ryll 0.2.0 is held until this
  phase lands, and is cut separately.
- **Changing ryll's default compression preference.** It stays
  `AUTO_GLZ`, for the measured reason recorded at
  `display.rs:1083`.

## What the survey found

Checked on 2026-10-10 against ryll `develop`. That includes phase 2's
merge `3763158`. The survey also read andris `a9e79cf`, kerbside's
ryll pin `fdb5ead5`, and the reference sources under
`/srv/src-reference/spice/`.

Path abbreviations, in ryll unless they say otherwise:

- C is `shakenfist-spice-compression/src`.
- P is `shakenfist-spice-protocol/src`.
- R is `shakenfist-spice-renderer/src/channels`.
- S is spice-server (`/srv/src-reference/spice/spice/server`).
- SC is spice-common (`/srv/src-reference/spice/spice-common`).

1. **Ryll has never received an LZ4 image.**
   - What ryll asks for:
     - Ryll advertises `DISPLAY_LZ4_COMPRESSION` (P/constants.rs:146,
       in `DEFAULT_DISPLAY`).
     - It then sends `PREFERRED_COMPRESSION` = `AUTO_GLZ`. That value
       is hard-coded at R/display.rs:1083, and there is no option to
       change it.
   - What spice-server needs before it sends LZ4 (S/dcc.cpp:582-645,
     680-687):
     - the client's effective setting must be exactly `LZ4` (7);
     - the client must advertise cap 5.
   - What spice-server does otherwise:
     - The `AUTO_*` modes never choose LZ4.
     - A non-RGB bitmap falls back to LZ.
     - A bitmap with stride padding is sent uncompressed.
     - A Unix-socket client gets no compression at all
       (S/dcc-send.cpp:439-441).
   - QEMU's `image-compression` option has no `lz4` value. Checked
     with `qemu-system-x86_64 -spice help`, the man page, and the
     strings in `ui-spice-core.so`.
   - So LZ4 reaches a client only when the client asks for it.
     Ryll's LZ4 decoder has only ever been tested against ryll's own
     test encoder.
   - Ryll's `docs/libvirt-spice-recommendations.md:200-203` says
     that, with `auto_lz` and the LZ4 cap, "the server will pick
     LZ4". That is wrong.
2. **The reference LZ4 framing**, read from S/lz4-encoder.c:45-122,
   S/image-encoders.cpp:1105-1150 and SC/common/canvas_base.c:517-614:
   - The image is a `BinaryData` (spice.proto:558-561, 612-613). After
     the 18-byte descriptor comes a `u32` little-endian `data_size`,
     then `data_size` bytes:
     - **Byte 0:** top-down, 0 or 1.
     - **Byte 1:** a `SPICE_BITMAP_FMT_*` value. In practice that is 6
       (16-bit, x555), 7 (24-bit, B,G,R), 8 (32-bit, B,G,R,X) or 9
       (RGBA, B,G,R,A).
     - **Then one or more blocks.** Each is a **big-endian** `u32`
       length, then one raw LZ4 *block*: no frame header, no
       checksum.
   - **The blocks are dependent.** spice-server compresses with one
     `LZ4_stream_t`, so a block may refer back into earlier blocks'
     input. spice-common decodes them with one `LZ4_streamDecode_t`
     into one contiguous buffer, and stops when the input runs out.
   - **How many blocks spice-server sends:** one per source chunk.
     It linearises unstable chunks first, so a typical image is a
     single block.
   - **Row order:** rows are encoded in memory order. The first
     decoded row is the top row when byte 0 is 1, and the bottom row
     when it is 0.
   - **Width and height come from the descriptor.** The encoded
     stride is `width * bytes_per_pixel`, with no padding: spice-server
     never LZ4-encodes a padded bitmap.
   - **spice-common does not check the total.** It never checks that
     the decoded size equals `height * stride`. Format 10 (`8BIT_A`)
     is refused.
3. **Ryll's decoder disagrees on all three counts** (C/lz4.rs:18-110,
   R/display.rs:2500-2512, as ryll#475 says):
   - **No `data_size`.** It reads no `data_size`, and the renderer
     passes it the bytes straight after the descriptor.
   - **Independent per-row blocks.** It decodes exactly `height`
     independent blocks, one per row, with `lz4_flex::decompress`.
     It would refuse spice-server's single block, and any dependent
     one.
   - **Format bytes that are not spice's.** It maps 0 and 4 to 32-bit,
     6 to RGBA, 3 to 24-bit and 2 to 16-bit. Those are not
     `SPICE_BITMAP_FMT_*` values. It refuses 7, 8 and 9, which are
     the values spice-server sends.
   - **What the history says.** Commit e5eab94, which dropped the
     `data_size`, says the bytes then looked like "a zlib header, not
     LZ4". They were probably not LZ4 at all.
   - **Ryll's other code already disagrees with the decoder:**
     - The protocol crate's `BinaryData` doc already says LZ4 carries
       one (P/messages/display.rs:697-698).
     - The renderer's comment says it does not.
     - The Pixmap path uses `bitmap_fmt` correctly
       (R/display.rs:2259, 2347).
4. **lz4_flex can do what spice-common does.** lz4_flex 0.14.0, already
   the decoder's dependency, has `block::decompress_into_with_dict` and
   `block::compress_with_dict`. Decoding block *n* into the output
   buffer, with the preceding output as the dictionary, reproduces
   `LZ4_decompress_safe_continue` on a contiguous buffer. Independent
   blocks decode the same way.
5. **The JPEG framing was never in doubt.** spice-server sends JPEG as
   a `BinaryData` holding a baseline JFIF stream (S/jpeg-encoder.c:225-277,
   S/image-encoders.cpp:1001-1061):
   - The encoding is quality 85 (S/dcc.cpp:47), 4:2:0 chroma, and
     always visually top-down.
   - spice-common **asserts** that the JPEG's own dimensions equal the
     descriptor's (SC/common/canvas_base.c:469-497). It decodes into
     x8r8g8b8.
   - Ryll strips the `data_size` correctly (R/display.rs:2557-2588).
   - The protocol crate already models `ImagePayload::Jpeg(BinaryData)`
     with a writer.
   - spice-server chooses JPEG only for lossy DRAW_COPY on a link it
     has measured as under 10 Mbit/s. Ryll's JPEG decoding is
     therefore far less exercised than its GLZ decoding, but nothing
     suggests its framing is wrong.
6. **No encoder exists, outside tests.** The compression crate:
   - is version 0.1.7;
   - has default features `quic, glz, lz, lz4, jpeg, mozjpeg`, and no
     `encode` feature;
   - has no dev-dependencies.
   Existing encoding code:
   - **LZ4:** only the test helper `encode_spice_lz4`
     (C/lz4.rs:159-182), which writes ryll's per-row format.
   - **JPEG:** only in tests, through mozjpeg. Production JPEG
     decoding goes through `best_for_platform()` (C/jpeg.rs:1478).
   - **Property tests:** none anywhere in ryll. Neither proptest nor
     quickcheck is a dependency.
   - **Compression fuzz targets:** none. The only cargo-fuzz crate is
     the protocol crate's, and `.github/workflows/fuzz.yml:201` runs
     that one manifest.
   - **Compression fixtures:** one, a 32x32 JPEG swatch.
7. **`make test` and `make lint` build only default features.** That
   is `cargo test --workspace` and
   `cargo clippy --workspace --all-targets` (Makefile:267-276). A
   non-default `encode` feature would be neither tested nor linted
   unless the Makefile asks for it.
8. **A capture is possible on this host.**
   - The pieces:
     - Debian's libspice-server 0.15.2 is built with liblz4.
     - `make test-qemu` (Makefile:440-456) runs QEMU on the host with
       QXL. Its SPICE port is 5900, with no ticket, and its QMP socket
       is `/tmp/ryll-test-qemu-qmp.sock`.
     - Ryll's `--capture DIR` writes one pcap per channel, holding
       every received byte after TLS (ryll/src/capture.rs).
   - The caveats:
     - The capture queue holds 1024 items, and it counts drops
       rather than blocking.
     - `tools/pcap-inspect.py` does not know image type 109 (LZ4).
     - `remote-viewer --spice-preferred-compression=lz4` is a second
       client that asks for LZ4. Capturing its traffic needs a tee
       relay or root, because `dumpcap` is not usable by this user.
9. **Kerbside is not affected.**
   - It depends only on `shakenfist-spice-protocol`, and uses only
     `messages::MessageHeader` and `make_message`.
   - It does not use the compression crate or `ImagePayload`.
10. **Andris can depend on ryll by git revision, and not by a local
    path.**
    - Andris has no dependencies yet.
    - Its `deny.toml` (lines 60-68) already allows git sources from
      `https://github.com/shakenfist/ryll`, pinned by `rev`, which is
      how kerbside depends on ryll.
    - A path `[patch]` to a sibling ryll worktree cannot work as the
      master plan said: andris's Makefile mounts only the andris
      checkout and the cargo cache into its build container
      (Makefile:46-60).
11. **Releasing ryll needs a person.** The operator is holding
    ryll 0.2.0 until this phase lands.
    - The process (`docs/releasing.md`): `make propose-release`,
      then `make tag-release`, then `release.yml`.
    - Both scripts prompt for confirmation. `sign-tag`, which gates
      crates.io publication, waits for approval in the `release`
      environment.
    - Ryll is at 0.1.7, with 698 commits since that tag.
    - The release documentation is stale in places:
      - it names four crates where six publish;
      - `tools/propose-release.sh:31-36` checks only four;
      - it mentions a TestPyPI job that no longer exists.
12. **Smaller stale facts in ryll's documentation:**
    - `docs/spice-protocol.md:233` and `:289-290` say JPEG is decoded
      "via the `image` crate".
    - The compression crate's README calls LZ4 "per-row".

The master plan's phase 3 section, its phase 4 section and its
planning-effort note are corrected at source in this plan's commit,
for findings 1, 2, 10 and 11. Do not redo that.

## Decisions

1. **Code in ryll, plan in andris.** As in phase 2, there is one ryll
   pull request, and it links here.

2. **Settle the framing from a capture, with an independent decoder.**
   - Ryll gains a `--preferred-compression` option, which defaults to
     today's `auto-glz`. A headless ryll asks for LZ4 and captures
     spice-server's traffic from `make test-qemu`.
   - The captured payloads are decoded in Python with the `lz4`
     package. It wraps liblz4, the library spice-common itself uses,
     and its `lz4.block.decompress(..., dict=...)` reproduces
     streaming decode.
   - Each decoded image is compared with a QMP `screendump` of the
     same screen.
   - This settles the framing before any Rust is written, and
     without trusting the decoder under suspicion.
   - Why ryll and not remote-viewer: ryll records bytes before it
     decodes them, it runs headless, and the option stays useful. It
     is the only way to exercise ryll's LZ4 path against spice-server
     later. spice-common's source is the reference for decoding; a
     second client would add little.

3. **Fixtures are liblz4's output, not the encoder's.** The decoder's
   tests decode:
   - the captured spice-server payloads, compared with the
     screendump's pixels; and
   - synthetic streams made with liblz4 (via Python) that the capture
     may not contain: several dependent blocks, a bottom-up image, and
     the 16-, 24- and RGBA formats.

   The synthetic images come from a pixel formula that the Rust test
   recomputes, so no expected-pixel blob is needed for them. Fixtures
   stay under 256 KiB in total. A decoder that passes only against
   ryll's own encoder proves nothing, which is ryll#475's whole
   lesson.

4. **The decoder follows spice-common, and is stricter in two
   places.**
   - It takes the `BinaryData` body and the descriptor's width and
     height.
   - It reads top-down and format.
   - It decodes blocks into one buffer of
     `height * width * bytes_per_pixel`, each with the preceding
     output as its dictionary.
   - It converts to ryll's RGBA, flipping row order when the image is
     bottom-up. Alpha is kept only for format 9.
   - **First strictness:** the total decoded size must equal the
     buffer exactly, where spice-common does not check.
   - **Second strictness:** a block length that runs past the data
     is refused.
   - Failure stays all-or-nothing (`None`), as today.
   - The per-row format is dropped, not kept beside it. No known
     server sends it: ryll never asked for LZ4, and spice-server
     never sent it.

5. **The protocol crate gains `ImagePayload::Lz4(BinaryData)`.**
   - LZ4 images then read and write as typed values, like JPEG.
   - The bytes inside stay opaque to the protocol crate, which does
     not depend on the compression crate.
   - The renderer reads the `BinaryData` and hands its body to the
     decoder.
   - `fuzz_client_bound` covers the new variant through `DrawCopy`
     with no change.

6. **The encoders take a borrowed, strided BGRX image.**
   - Andris captures X11 ZPixmaps at depth 24, which are B,G,R,X in
     memory with a stride. That is also `SPICE_BITMAP_FMT_32BIT`'s
     layout. So:

     ```rust
     pub struct Bgrx<'a> {
         pub data: &'a [u8],
         pub width: u32,
         pub height: u32,
         pub stride: usize,
     }
     ```

     Its constructor validates that `stride >= width * 4` and
     `data.len() >= stride * (height - 1) + width * 4`, and that the
     size is within `limits`.
   - Each encoder returns the `BinaryData` body, so the caller wraps
     it in `ImagePayload::Lz4` or `ImagePayload::Jpeg`.
   - **LZ4** writes top-down 1 and format 8. It packs the rows to
     remove stride padding, because spice-common assumes there is
     none, and writes the whole image as one independent block, like
     spice-server's common case.
   - The encoder says nothing about when to give up. A caller whose
     output is longer than the raw pixels should send an
     uncompressed `BITMAP`, as spice-server does. The doc comment
     says so.
   - **JPEG** takes a quality argument; spice-server uses 85. It
     encodes 4:2:0 baseline from the BGRX input, ignoring X.

7. **JPEG uses the `jpeg-encoder` crate, not mozjpeg.**
   - Why `jpeg-encoder`:
     - It is pure Rust, so andris's build container needs no C
       toolchain for it.
     - Its encoder interface takes a custom image buffer, so it can
       read strided BGRX without a copy.
   - The cost:
     - Its licence is `(MIT OR Apache-2.0) AND IJG`. Ryll's
       `deny.toml` needs an IJG exception for it, as it already has
       for mozjpeg. Andris's will need one in phase 4.
     - It is probably slower than libjpeg-turbo's SIMD paths.
   - Phase 5 measures encode time per frame. If `jpeg-encoder` is too
     slow there, only the inside of `encode_spice_jpeg` changes,
     behind the same signature.
   - The step 5 sub-agent confirms the crate's current API before
     relying on it. It checks for an `ImageBuffer`-style trait, BGRA
     or BGRX input, and a 4:2:0 sampling setting. If any is missing,
     it stops and reports.

8. **No ryll release in this phase; 0.2.0 waits for it.**
   - **The operator is holding ryll 0.2.0** until this phase's
     breaking changes land, so one release carries both phase 2's
     wire types and this phase's. Those changes are
     `decompress_spice_lz4`'s input, and a new `ImagePayload` variant.
   - **Cutting the release is not a step of this plan.** It needs two
     human approvals, and its timing is ryll's call.
   - **Andris pins a git revision.** From phase 4 onward andris
     depends on ryll by git revision, exactly as kerbside does.
     Survey finding 10 shows this already passes andris's
     `cargo deny`.
   - **Andris moves to crates.io versions** once a release contains
     what it needs. Phase 10, packaging, is the latest point at which
     that matters.

9. **Property tests use proptest**, as a dev-dependency of the
   compression crate. Its licence is MIT or Apache-2.0, which ryll's
   `deny.toml` allows. Each test case is bounded to keep `make test`
   fast: images up to 64x64, padding up to 16 bytes, and the default
   256 cases.

10. **The `encode` feature is built, tested and linted in CI.**
    - `make test` and `make lint` add
      `--features shakenfist-spice-compression/encode`.
    - Ryll's CI runs `make test` and `make lint` (ci.yml:99, :163), so
      CI picks this up. The per-platform `cargo test` matrix
      (ci.yml:512) is left alone.
    - `encode` depends on `lz4_flex` and `jpeg-encoder` only, so a
      server can use it without default features.

11. **One fuzz target for the new decoder**, in the protocol crate's
    fuzz crate: the one manifest `fuzz.yml` runs.
    - `fuzz_lz4_decode` takes width and height from the first bytes,
      capped at 256, and the rest as the `BinaryData` body.
    - It asserts the decoder does not panic, and that what it accepts
      is exactly `width * height * 4` bytes.
    - Placing a compression target in the protocol crate's fuzz crate
      is untidy. It is still cheaper than a second fuzz manifest and
      workflow.

12. **Kerbside is checked by a search, not by running its tests.** By
    survey finding 9, nothing kerbside uses changes. Step 7 repeats
    the search against the branch. If the result differs, step 7
    runs kerbside's tests as phase 2 did.

## Execution

Steps 1 to 6 are commits on the ryll branch
`x11-desktop-phase-03-encoders`, in a worktree at
`/srv/kasm_profiles/mikal/vscode/src/shakenfist/ryll-wt-p03`. The
management session makes them after reviewing each sub-agent's diff.

- Every commit must leave `make lint` and `make test` passing in ryll.
- Cargo runs only through the `Makefile`, never on the host.
- Step 2 runs QEMU and a built ryll on the host, as `make test-qemu`
  already does.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 0 | low | management | none | Not started | Land this plan; create the ryll worktree. See brief 0. |
| 1 | medium | sonnet | none | Not started | `--preferred-compression`, and LZ4 in `pcap-inspect.py`. See brief 1. |
| 2 | high | opus | none | Not started | Capture spice-server LZ4, settle the framing, and build the fixtures. See brief 2. |
| 3 | high | opus | none | Not started | Rewrite the LZ4 decoder; add `ImagePayload::Lz4`; re-point the renderer. See brief 3. |
| 4 | medium | sonnet | none | Not started | The `encode` feature and the LZ4 encoder, with property tests. See brief 4. |
| 5 | medium | sonnet | none | Not started | The JPEG encoder. See brief 5. |
| 6 | medium | sonnet | none | Not started | The `fuzz_lz4_decode` target, and ryll documentation. See brief 6. |
| 7 | medium | management | none | Not started | Push audit, kerbside check, pull request, queue. See brief 7. |

Step 2 is high effort because its result is a judgement: what
spice-server actually sends. Every later step builds on that answer.
Step 3 is high effort because it replaces a decoder that ryll's
client runs on untrusted input. In every brief, ryll paths are
relative to the ryll worktree, and C, P and R abbreviate as in the
survey.

**Brief 0: land the plan** (management session).

- Commit the phase 2 close-out, then this plan, on this andris
  branch. Open a pull request and put it through andris's merge
  queue.
- Create the ryll worktree:
  `git -C ../ryll worktree add -b x11-desktop-phase-03-encoders
  ../ryll-wt-p03 origin/develop`.
- Remove `../andris-wt-p02`. Its one unpushed commit is carried in
  the close-out.

**Brief 1: ask for a compression, and see LZ4 in captures.**

- In `ryll/src/config.rs`, add `--preferred-compression <MODE>` to
  `Args`.
  - Modes: `off`, `auto-glz`, `auto-lz`, `quic`, `glz`, `lz`, `lz4`.
    They map to `image_compression::*` in P/constants.rs:354-363.
  - The default is `auto-glz`.
  - Use clap's `ValueEnum`, following the other enum-valued options
    in that file.
- Thread the value through to the display channel, the way the
  other per-session display settings reach it. R/display.rs:1083
  sends the configured value instead of the literal `AUTO_GLZ`. Keep
  the comment explaining why `auto-glz` is the default.
- Add a test that the default sends `AUTO_GLZ` (2) and that `lz4`
  sends 7. Use the display channel's loopback test harness, as the
  `stream_report_bytes_are_unchanged` test does.
- In `tools/pcap-inspect.py`, add 109 as `LZ4` to `IMAGE_TYPES`
  (lines 77-89). Have `draw-copy` print, for LZ4 images:
  - `data_size`;
  - the top-down and format bytes;
  - the count and lengths of the big-endian-length blocks.

  Use single quotes, per the house Python style.
- Document the option in `docs/` wherever ryll's command-line
  options are listed. Say it exists for diagnostics, and that
  `auto-glz` is the default for a measured reason.

Commit subject: `Let ryll ask for an image compression.`

**Brief 2: capture spice-server's LZ4 and settle the framing.**

The deliverable is a findings report and fixture files, not a
commit. The files are left uncommitted in the worktree. Step 3
commits them with the decoder that uses them.

1. Build ryll with `make build`. Run `make test-qemu`, which downloads
   its guest image on first use.
2. Run the built ryll headless against `localhost:5900` with
   `--preferred-compression lz4 --capture <scratch dir>`. Read
   `ryll --help` for the headless and direct-connect options; phase
   2's step 9 ran ryll this way.
   - The preference is sent after the display INIT, so the first
     full-screen draw may arrive as GLZ. Force later redraws by
     sending keys through QMP (`send-key` on
     `/tmp/ryll-test-qemu-qmp.sock`). The guest repaints its screen
     on each keystroke.
   - When the screen settles, take a QMP `screendump` to a PPM file.
3. Check that the capture's `metadata.json` reports no dropped
   packets. Then list the DRAW_COPY images with
   `tools/pcap-inspect.py draw-copy`. If none is type 109, try
   `make test-qemu-desktop` (an XFCE guest; see Makefile:497-510),
   and report which guest gave LZ4.
4. In a Python venv in the scratchpad, install `lz4`. Then, for each
   LZ4 image:
   - check that `data_size` equals the bytes remaining in the image;
   - check that byte 0 is 0 or 1, and record byte 1;
   - check that the big-endian block lengths, each plus 4, sum to
     `data_size - 2`;
   - decode the blocks in order with `lz4.block.decompress(block,
     uncompressed_size=..., dict=<all output so far>)`, into a
     buffer of `height * width * bpp`;
   - compare the result, converted to RGB, with the screendump
     cropped to the draw's destination rectangle.

   Report every check's result. If anything disagrees with survey
   finding 2, **stop and report**. Do not adapt the fixtures to fit.
5. Write fixtures under
   `shakenfist-spice-compression/tests/fixtures/lz4/`.
   - **Captured images.** Keep up to three, smallest first, under
     192 KiB together. For each:
     - `<name>.bin`, the `BinaryData` body (the bytes after
       `data_size`);
     - `<name>.rgba.lz4`, the screendump's matching RGBA pixels,
       compressed with `lz4.block.compress(store_size=True)`. That is
       lz4_flex's `compress_prepend_size` layout;
     - its width and height, recorded in `README.md`.
   - **Synthetic streams**, made with the `lz4` package from the
     formula `r = (x * 7 + y) & 0xff`, `g = (y * 13) & 0xff`,
     `b = (x ^ y) & 0xff`, and `a = (x + y * 3) & 0xff` for RGBA,
     else 255:
     - 32-bit top-down, three dependent blocks of 5, 5 and 6 rows
       (each compressed with `dict=` the preceding input);
     - 32-bit bottom-up, one block;
     - 24-bit, 16-bit (x555) and RGBA, top-down, one block each.

     All are 37 pixels wide and 16 rows high, so the formula wraps
     and rows differ. Name them so the test can find them.
   - Write `README.md` with:
     - what each file is, and how it was made;
     - the guest, and spice-server's version (`dpkg -l
       libspice-server1`);
     - the Python script used, inline.
6. Stop QEMU with `make test-qemu-stop`.

Report the checks, the fixture list and sizes, and anything
surprising. The management session records the result in this
plan's "Capture" section.

**Brief 3: rewrite the LZ4 decoder.**

Read survey findings 2 to 4 and decision 4, and the fixtures'
`README.md` from step 2.

- **Rewrite `decompress_spice_lz4` in C/lz4.rs.**
  - It keeps the same signature, but `data` is now the `BinaryData`
    body.
  - It reads the top-down byte and format byte. The compression
    crate does not depend on the protocol crate, so define the four
    format values locally, with a comment citing `enums.h:216-229`.
  - Supported formats:

    | Format | Bytes per pixel | Layout |
    |--------|-----------------|--------|
    | 6 | 2 | x555 LE |
    | 7 | 3 | B,G,R |
    | 8 | 4 | B,G,R,X; alpha 255 |
    | 9 | 4 | B,G,R,A |

    Any other value returns `None`.
  - Size the output with checked arithmetic and `limits::rgba_len`.
  - Loop:
    - read a big-endian `u32` length, and refuse it if it runs past
      the data;
    - call `lz4_flex::block::decompress_into_with_dict(block,
      &mut out[pos..], dict)`, where `dict` is
      `&out[pos.saturating_sub(65536)..pos]`;
    - advance by what it decoded.

    The output buffer and the dictionary alias, so decode into a
    separate scratch slice and copy, or split the buffer with
    `split_at_mut`. Check how lz4_flex's API takes them.
  - Require `pos` to equal the buffer exactly at the end.
  - Convert to RGBA. Bottom-up images reverse row order.
  - 16-bit expands each 5-bit channel with `(v << 3) | (v >> 2)`.
- **Replace the test-only encoder `encode_spice_lz4`.** Its tests
  must not depend on step 4's encoder, so build their inputs with
  `lz4_flex::block::compress` by hand, in the new framing. Remove
  tests that encode the old per-row framing.
- **Add fixture tests** that decode every file from step 2:
  - The captured images must equal their `.rgba.lz4` pixels exactly,
    since LZ4 is lossless.
  - The synthetic ones must equal the formula.
  - Add truncation tests: the body cut at each block boundary and in
    the middle of a block returns `None`. So does an unknown format.
- **In P/messages/display.rs, add `ImagePayload::Lz4(BinaryData)`.**
  - Read it when the type is LZ4, and write it, following
    `ImagePayload::Jpeg`.
  - Remove LZ4 from `Other`'s doc.
  - Add a round-trip test, and a test that the existing LZ4 bytes in
    the test near line 2185 now read as `Lz4`.
- **In R/display.rs:2500-2512,** read `BinaryData` from `image_data`,
  as the JPEG arm at :2557-2588 does, and pass its body. Delete the
  comment that says there is no `data_size`. A body that is too
  short warns with the key `display:decode_failure:lz4:short_data`,
  following the JPEG arm's `jpeg:short_data`.
- **Docs.** In `docs/spice-protocol.md`:
  - replace the LZ4 description and the discrepancy note (around
    lines 237, 276-280 and 344-368) with the framing from survey
    finding 2, citing spice.proto and enums.h;
  - fix the JPEG "via the `image` crate" wording at :233 and
    :289-290.
- In the compression crate's README, drop "per-row".

The commit message says `Fixes #475`.

Commit subject: `Decode LZ4 images as spice-server sends them.`

**Brief 4: the `encode` feature and the LZ4 encoder.**

- **In C's `Cargo.toml`:**
  - add `encode = ["dep:lz4_flex", "dep:jpeg-encoder"]`;
  - add `jpeg-encoder` as optional. Leave its use to step 5. If the
    unused optional dependency trips a lint, add it in step 5
    instead;
  - add `proptest` under `[dev-dependencies]`;
  - run `make lock` (network) to update `Cargo.lock`.
- **In the Makefile,** make `test`, `lint` and `lint-fix` pass
  `--features shakenfist-spice-compression/encode` (Makefile:267-283).
- **Add `C/encode.rs`**, compiled under `feature = "encode"`, holding:
  - `Bgrx`, as in decision 6, with a `new` that validates and
    returns `Result`;
  - `encode_spice_lz4(&Bgrx) -> Vec<u8>`.
  `encode_spice_lz4` writes `[1, 8]`, then one big-endian length, then
  `lz4_flex::block::compress` of the packed rows. Its doc comment
  covers:
  - the fall-back-to-`BITMAP` rule in decision 6;
  - that the result is a `BinaryData` body.
- **Tests**, under `cfg(all(test, feature = "encode", feature =
  "lz4"))`:
  - a fixed image's bytes;
  - proptest round trips through `decompress_spice_lz4`. Width and
    height are 1 to 64, and the stride padding 0 to 16 bytes, filled
    with garbage that must not appear in the output. X becomes alpha
    255;
  - `Bgrx::new` refuses a short buffer, a short stride and zero
    size.
- **Add a proptest that `decompress_spice_lz4` never panics** on
  arbitrary bytes. It goes in C/lz4.rs, compiled under
  `feature = "lz4"` and needing no encoder.
- Export the encoder from `lib.rs` under the feature. Document the
  feature in the crate's README.

Commit subject: `Add an LZ4 image encoder.`

**Brief 5: the JPEG encoder.**

- **First, check `jpeg-encoder`'s current API** (decision 7). It must
  provide all of:
  - a trait for custom image input;
  - BGRX or BGRA input;
  - quality;
  - 4:2:0 sampling.

  If it lacks any of these, **stop and report** before writing code.
- **Add `encode_spice_jpeg(&Bgrx, quality: u8) -> Result<Vec<u8>>`**
  to `C/encode.rs`.
  - It returns the JFIF bytes, which are the `BinaryData` body.
  - It feeds the strided BGRX through the crate's custom-input trait,
    with no intermediate copy.
  - It sets 4:2:0.
  - Its doc comment notes that spice-server uses quality 85, and that
    the client asserts the JPEG's dimensions equal the descriptor's.
- **Add the deny exception.** In `deny.toml`, add
  `{ allow = ["IJG"], crate = "jpeg-encoder" }` beside the mozjpeg
  entries (deny.toml:84-94), with a comment in their style.
- **Tests:**
  - Decode the output with both `JpegDecoderRsDecoder` and, where
    built, `MozJpegDecoder`.
  - Assert the dimensions match exactly. Assert a mean absolute
    error per channel under 3 for a smooth gradient at quality 85,
    and under 1 for a solid colour.
  - A proptest over sizes 1 to 64 and stride padding asserts the
    decode succeeds and the dimensions match.
  - Quality 1 and 100 both produce decodable output.

Commit subject: `Add a JPEG image encoder.`

**Brief 6: fuzz target and documentation.**

- **Fuzz target.** In `shakenfist-spice-protocol/fuzz/Cargo.toml`,
  add a path dependency on the compression crate with
  `default-features = false, features = ["lz4"]`. Add a
  `fuzz_lz4_decode` `[[bin]]` per decision 11, following
  `fuzz_link_mess_parse`.
  - Run `make fuzz-fmt-check` and
    `make fuzz-smoke-fuzz_lz4_decode`.
  - Check that `tools/fuzz-targets.sh
    shakenfist-spice-protocol/fuzz/Cargo.toml` lists the target.
- **Fix `docs/libvirt-spice-recommendations.md:200-203`** (survey
  finding 1). Say that the AUTO modes never choose LZ4, and that a
  client must ask for it.
- **`docs/spice-protocol.md`:** add a short "Encoding images (server
  role)" passage beside the server-role section phase 2 added. It
  covers:
  - the `encode` feature and the two encoders;
  - the `BinaryData` wrapping;
  - the fall-back-to-`BITMAP` rule;
  - that LZ4 needs the client's cap 5;
  - that JPEG's dimensions must match the descriptor's.
- **`AGENTS.md`:** if it lists the compression crate's features or
  says the crate is decode-only, update that sentence. Otherwise
  leave it alone.
- **`ARCHITECTURE.md:30`:** same rule.

Commit subject: `Fuzz the LZ4 decoder and document the encoders.`

**Brief 7: audit, kerbside, pull request** (management session).

1. Run ryll's `PUSH-AUDIT.md` over the branch against `develop`.
2. Run `make lint test` and `make fuzz-fmt-check`.
3. Search kerbside for anything that changed:
   `grep -rn "ImagePayload\|shakenfist_spice_compression\|decompress_spice_lz4"
   ../kerbside/rust`. If it finds anything, run kerbside's tests at
   this branch's revision, as phase 2's decision 12 did.
4. Ask the operator before opening the ryll pull request. Its body:
   - links this plan;
   - says `Fixes #475`;
   - lists the breaking changes (`decompress_spice_lz4`'s input and
     the new `ImagePayload` variant), which ship in ryll 0.2.0;
   - lists the behaviour change: ryll now decodes spice-server's LZ4,
     and no longer decodes the per-row format.
5. Watch ryll's smoke tier and automated reviewer, then the merge
   queue.
6. Record the ryll merge commit in this file's Result section. The
   master plan's `Merged` cell is filled in by phase 4's planning
   commit.
7. Offer to file a ryll issue for the stale release documentation
   in survey finding 11.

## Capture

Not yet run. Step 2's findings go here.

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| The test guest's screen never produces an LZ4 image: wrong format, padding, or too small. | Brief 2 falls back to the XFCE guest. If neither works, the synthetic liblz4 fixtures still pin the decoder to liblz4's semantics. The management session then records that the framing was settled from source, and not confirmed by capture. |
| The capture disagrees with the source reading. | Brief 2 stops rather than fitting fixtures to it. The management session re-plans step 3 from what the capture shows. |
| The new decoder breaks LZ4 for some server that sends the per-row format. | None is known: ryll never asked for LZ4, and spice-server never sent that format. The pull request lists the change. |
| `jpeg-encoder` lacks an API the encoder needs. | Brief 5 checks first and stops. The fallback is mozjpeg's compressor, already in ryll's tree, at the cost of a C build for andris. |
| The `encode` feature rots because the defaults do not build it. | Decision 10 puts it in `make test` and `make lint`, which CI runs. |
| Proptest slows `make test`. | Decision 9 bounds the sizes. The management session compares test time with `develop`'s. |

## Definition of done

- **The capture settled the framing.**
  - This file's "Capture" section records the spice-server version
    and guest.
  - It records that liblz4 decoded the captured images and that the
    pixels matched the screendump.
  - If no capture produced LZ4, the section says so, and says which
    fallback was used.
- **The decoder reads spice-server's bytes.**
  - The fixture tests pass, against both the captured and the
    synthetic fixtures.
  - `grep -n "per-row\|encode_spice_lz4" C/lz4.rs` finds nothing:
    the per-row framing and its test encoder are gone.
  - `grep -n "does NOT have a data_size" R/display.rs` finds nothing.
- **The encoders round-trip.** Running `make test` shows the
  `encode` tests and the proptests in the compression crate's
  output.
- **The `encode` feature is linted.** `grep -n "compression/encode"
  Makefile` matches the `test` and `lint` targets.
- **The fuzz target exists and smoke-runs.**
  `tools/fuzz-targets.sh shakenfist-spice-protocol/fuzz/Cargo.toml`
  lists `fuzz_lz4_decode`, and `make fuzz-smoke-fuzz_lz4_decode`
  passes.
- **ryll#475 is closed** by the merge.
- **No wrong LZ4 claim survives in ryll's docs.**
  `grep -rn "LZ4" docs/ shakenfist-spice-compression/README.md` shows
  nothing that says per-row, no `data_size`, or that AUTO modes
  choose LZ4.
- **No new message names.** `git diff --exit-code origin/develop --
  shakenfist-spice-protocol/src/logging.rs` succeeds.
- **Ryll's checks pass.** The ryll pull request passed its smoke tier
  and merge queue.

## Result

Not yet run.

## Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan.
