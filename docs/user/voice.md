# Voice downloads

Voice runs locally after its optional models are installed. The two parts download separately:

| Part | Downloads | Installed size |
|---|---|---|
| Speech input | Parakeet TDT 0.6B v3, int8 | 670 MB |
| Spoken replies | Phonon 31M (Q8), its tokenizer and configuration, and the Juniper voice | 65 MB |

Sizes are decimal megabytes. Files come from pinned revisions on Hugging Face and its
`hf.co` download servers, without an account or credentials. Every file is checked against
its expected size and SHA-256 before use. The native runtime ships with the app, not as a
download.

Downloads need space for the remaining bytes plus 10%. Cancelled or interrupted downloads
keep their partial files for a later explicit attempt; they never resume just because the app
starts. Installation controls are not connected yet.
