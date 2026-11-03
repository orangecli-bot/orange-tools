# Third-party notices

The code in this repository is MIT-licensed (see LICENSE). It contains no code copied from the
Orange app (ramp-stack/orange). Each crate does include a small, wire-compatible re-declaration
of the `ChatRoom` contract (the contract id, the `Init` type, and the first two `ChatRoomAction`
variants, `Share` and `SendMessage`). It mirrors `src/air/messages.rs` in
[ramp-stack/chk](https://github.com/ramp-stack/chk) (rev 2da2ba1), which is distributed under the
BSD 3-Clause License below. That license's notice is kept here for those portions.

Dependencies are pulled from crates.io at build time and are not vendored. Their licenses are all
compatible with MIT, for example:
- `air` 4.0.1 and `air_server` 2.0.1 (ramp-stack): BSD-3-Clause
- `crossfire` 3.1.7: Apache-2.0
- `tokio`, `serde`, `serde_json`, `postcard`, `chrono`, `hex`, `libc`, `rustls`, `tokio-rustls`: MIT and/or Apache-2.0
- `webpki-roots`: CDLA-Permissive-2.0 (Mozilla CA root data)

---

BSD 3-Clause License

Copyright (c) 2025, ramp-stack

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived from
   this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
