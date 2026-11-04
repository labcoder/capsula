# Voice native runtime

Speech input runs Parakeet through sherpa-onnx's C API, loaded at run time from beside
`kumochid.exe`. Nothing links to it at build time, so builds, tests and CI never need it, and
voice stays optional. Loading it at run time doesn't change what shipping it obliges, which is
what this page records.

## The artifact

- **Source:** `sherpa-onnx-v2.0.1-win-x64-shared-MD-Release-lib.tar.bz2` from
  [sherpa-onnx v2.0.1](https://github.com/k2-fsa/sherpa-onnx/releases/tag/v2.0.1), SHA-256
  `c3c78220db8bebf97d5f932496165bb3897b163410427d7c021013439c80f183` as the release page
  lists it. It's the CPU build against the dynamic VC runtime (`/MD`).
- **Shipped:** `sherpa-onnx-c-api.dll` and `onnxruntime.dll` (ONNX Runtime 1.28.2), pinned by
  size and SHA-256 in `kumochi_protocol::voice::NATIVE_FILES`. The brain checks the hash before
  it loads either one.
- **Not shipped:** `sherpa-onnx-cxx-api.dll`, which Kumochi doesn't call, the `.lib` import
  libraries, and `onnxruntime_providers_shared.dll`, which ORT loads only for shared execution
  providers such as CUDA and which no import table names.

## Imports

Read with `dumpbin /dependents` on Rez, 2026-11-04:

| DLL | Imports beyond Windows |
|---|---|
| `sherpa-onnx-c-api.dll` | `onnxruntime.dll`, `MSVCP140.dll`, `VCRUNTIME140.dll`, `VCRUNTIME140_1.dll`, UCRT `api-ms-win-crt-*` |
| `onnxruntime.dll` | `MSVCP140.dll`, `MSVCP140_1.dll`, `VCRUNTIME140.dll`, `VCRUNTIME140_1.dll`, UCRT; Windows' `dbghelp.dll`, `SETUPAPI.dll`, `dxgi.dll` |

The VC++ 2015–2022 runtime is a prerequisite, and slice 10's clean-machine rehearsal proves it.
Import tables don't show libraries loaded with `LoadLibrary`, so the Parakeet measurement records
the modules that appear after load, warm-up and decodes. On Rez (2026-11-04) every one besides the two
pins came from `System32`: `combase`, `msvcp_win`, `win32u`, `gdi32`, `gdi32full`, `USER32`, `IMM32`,
`DEVOBJ`, `cfgmgr32`, `WINTRUST`, `MSASN1`, `dxcore`, `kernel.appcore`, `SETUPAPI`, `MSVCP140`,
`MSVCP140_1`, `dbghelp`, `OLEAUT32`, `VCRUNTIME140_1`, `dxgi`, `powrprof`, `UMPDC` and
`directxdatabasehelper`.

**Its own output:** with `debug = 0`, sherpa-onnx still prints one line group to stderr each time
it resamples a non-16 kHz utterance: `offline-stream.cc:AcceptWaveformImpl:155 Creating a
resampler:` followed by the input and output sample rates. It names no file, text or audio. The
brain doesn't capture its own stderr today; whoever routes it later should drop that line.

## Licenses

| Component | Where | License | How it's known |
|---|---|---|---|
| sherpa-onnx | `sherpa-onnx-c-api.dll` | Apache-2.0 | its `LICENSE` |
| kaldi-native-fbank, kaldi-decoder, kaldifst | `sherpa-onnx-c-api.dll` | Apache-2.0 | linked, by symbol strings |
| OpenFst | `sherpa-onnx-c-api.dll` | Apache-2.0 | linked, by symbol strings |
| simple-sentencepiece | `sherpa-onnx-c-api.dll` | Apache-2.0 | linked, by symbol strings |
| ONNX Runtime | `onnxruntime.dll` | MIT | its `LICENSE` |
| protobuf, re2, abseil, flatbuffers, cpuinfo, ONNX, PyTorch-derived kernels | `onnxruntime.dll` | BSD-3-Clause, BSD-3-Clause, Apache-2.0, Apache-2.0, BSD-2-Clause, Apache-2.0, BSD-3-Clause | ORT's `ThirdPartyNotices.txt`, and symbol strings |
| Eigen | `onnxruntime.dll` | MPL-2.0 | ORT's notice; a named MPL-2.0 exception |
| Boost (header-only parts) | `onnxruntime.dll` | BSL-1.0 | ORT's notice; outside the agreed list, an open owner decision (item 13 of the 2026-11-04 decisions) |

**What was checked**, on Rez, 2026-11-04:
- **Direct imports** of both DLLs, with `dumpbin /dependents` (the table above).
- **ORT's retained `ThirdPartyNotices.txt`**, which lists Eigen (MPL-2.0) and Boost (BSL-1.0). It's
  a broad inventory: it also names MKL and XNNPACK, so a name there doesn't prove a component is
  compiled into this CPU build.
- **Symbol and string searches** of both DLLs, which found none of these:
  - **espeak-ng** (GPL-3.0) and piper-phonemize. sherpa-onnx dropped espeak-ng in v2.0.0, and the
    C API DLL carries no espeak API symbol, only the message saying it's no longer used.
  - **pthreads4w**, which Windows builds don't use.
  - **Intel MKL**, **oneDNN** and **XNNPACK**, execution providers this build doesn't include.
    Only their names appear, in ORT's provider list.

**What that doesn't prove:** an import table and the absence of recognizable strings don't
establish that no other library is statically linked. These checks found no GPL or LGPL code, but
they aren't a complete license closure for the two DLLs.

**Owed before these DLLs ship** (slice 10's installer):
- exact-build upstream dependency evidence for this release, or a matching SBOM;
- the complete notice bundle: ORT's full `ThirdPartyNotices.txt` and every other required upstream
  license and NOTICE text, beside the DLLs;
- a pinned Eigen source version, for MPL-2.0 source availability, rather than the project URL alone;
- the owner's decision on Boost's BSL-1.0 (item 13 of the 2026-11-04 owner decisions);
- the module-list check after load, warm-up and decode, and the clean-machine prerequisite check.

[THIRD-PARTY-NOTICES.md](../../THIRD-PARTY-NOTICES.md) lists each component.
