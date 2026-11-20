# Changelog

## [0.1.20](https://github.com/matt-riley/panda/compare/v0.1.19...v0.1.20) (2026-11-20)


### Features

* add allowlist-aware tool discovery to Codemode ([#62](https://github.com/matt-riley/panda/issues/62)) ([88e0f0a](https://github.com/matt-riley/panda/commit/88e0f0aac3dc2c8621cdd84a15245db1eafafd88))
* add Codemode-only tool exposure for print and JSON runs ([#63](https://github.com/matt-riley/panda/issues/63)) ([f892481](https://github.com/matt-riley/panda/commit/f892481d7d11ac33e268596585e7a482f1e8cbc3))
* export sessions as standalone HTML ([#59](https://github.com/matt-riley/panda/issues/59)) ([8974d44](https://github.com/matt-riley/panda/commit/8974d44bf73a90d8d938f2a57a06de81f2911efc))
* guide branch summaries from the session tree ([#54](https://github.com/matt-riley/panda/issues/54)) ([8f54d71](https://github.com/matt-riley/panda/commit/8f54d7106a7268b16add5fc8695e7fa5193c3b37))
* persist Codemode values on session branches ([#60](https://github.com/matt-riley/panda/issues/60)) ([0d7e7a4](https://github.com/matt-riley/panda/commit/0d7e7a4e6387a0dba5d1b52489bd5e92e379c257))
* preserve full Codemode output with bounded previews ([#61](https://github.com/matt-riley/panda/issues/61)) ([65d5df7](https://github.com/matt-riley/panda/commit/65d5df79f6a466f4f30c72e5340b17bba647a845))
* recover from early provider length stops ([#57](https://github.com/matt-riley/panda/issues/57)) ([b1e123a](https://github.com/matt-riley/panda/commit/b1e123a8b100737f63cf1c378a32a7b3ecd91092))
* retain recent exchanges during compaction ([#56](https://github.com/matt-riley/panda/issues/56)) ([1781132](https://github.com/matt-riley/panda/commit/17811329f03f45bbc25f271ef1fe11b3294cc837))
* track file activity across compactions ([#58](https://github.com/matt-riley/panda/issues/58)) ([fca444e](https://github.com/matt-riley/panda/commit/fca444e1630b9a8231f41d2fbd812d98783b9126))

## [0.1.19](https://github.com/matt-riley/panda/compare/v0.1.18...v0.1.19) (2026-11-19)


### Features

* accept instructions for manual compaction ([#53](https://github.com/matt-riley/panda/issues/53)) ([e028d78](https://github.com/matt-riley/panda/commit/e028d78e32e21164c268dd404c4034df74cd812e))
* edit and filter labels in the session tree ([#52](https://github.com/matt-riley/panda/issues/52)) ([294d958](https://github.com/matt-riley/panda/commit/294d958f1d17f5f7578eb9687dc126a5675c027b))
* fork a session from a user prompt in the tree picker ([#51](https://github.com/matt-riley/panda/issues/51)) ([ec4b84b](https://github.com/matt-riley/panda/commit/ec4b84b877f1dc56ecc7ea4d4927f6b9783fd323))
* import and export Pi v3 JSONL sessions ([#48](https://github.com/matt-riley/panda/issues/48)) ([388be1c](https://github.com/matt-riley/panda/commit/388be1cb9ae7cbeeba07d8cb996bb6828caff785))


### Bug Fixes

* continue the most recently modified workspace session ([#50](https://github.com/matt-riley/panda/issues/50)) ([b6ec819](https://github.com/matt-riley/panda/commit/b6ec81963524a0ad8f85f359e6589542bc61b508))

## [0.1.18](https://github.com/matt-riley/panda/compare/v0.1.17...v0.1.18) (2026-11-19)


### Features

* bound shell timeouts and preserve full command output ([#46](https://github.com/matt-riley/panda/issues/46)) ([dd961b9](https://github.com/matt-riley/panda/commit/dd961b92e586e1c680f21d9dca2e0eccd39891c3))

## [0.1.17](https://github.com/matt-riley/panda/compare/v0.1.16...v0.1.17) (2026-11-19)


### Features

* read workspace images in plan mode and Codemode ([#44](https://github.com/matt-riley/panda/issues/44)) ([b044fdb](https://github.com/matt-riley/panda/commit/b044fdbf0da212ebbbbfb17bfe07ae2b1b6aba20))

## [0.1.16](https://github.com/matt-riley/panda/compare/v0.1.15...v0.1.16) (2026-11-19)


### Features

* resume print and JSON runs by session ID ([#40](https://github.com/matt-riley/panda/issues/40)) ([97f1156](https://github.com/matt-riley/panda/commit/97f115621054c8083783bf531a8d97ee0b807361))
* select the session directory for print and JSON runs ([#41](https://github.com/matt-riley/panda/issues/41)) ([8604485](https://github.com/matt-riley/panda/commit/8604485dd40538e37354d437144bf7bcc26f6b5f))
* start interactive sessions with a positional prompt ([#42](https://github.com/matt-riley/panda/issues/42)) ([d08f177](https://github.com/matt-riley/panda/commit/d08f1771ce7a3b4f72f20d6f52c8f0e0b92b0c9b))
* support atomic single-file batch edits ([#43](https://github.com/matt-riley/panda/issues/43)) ([2109cde](https://github.com/matt-riley/panda/commit/2109cde66cc2cfe5a01847bdce39389947abb71c))
* support system prompt overrides for print and JSON runs ([#38](https://github.com/matt-riley/panda/issues/38)) ([ff66638](https://github.com/matt-riley/panda/commit/ff66638b3d7814a756408c045e25e320e2c1b09e))

## [0.1.15](https://github.com/matt-riley/panda/compare/v0.1.14...v0.1.15) (2026-11-19)


### Features

* support tool selection for print and JSON runs ([#36](https://github.com/matt-riley/panda/issues/36)) ([ded2efc](https://github.com/matt-riley/panda/commit/ded2efc93ed5b556ecda8b03334f484657049694))

## [0.1.14](https://github.com/matt-riley/panda/compare/v0.1.13...v0.1.14) (2026-11-19)


### Features

* add context and case-insensitive matching to search ([#33](https://github.com/matt-riley/panda/issues/33)) ([83299c6](https://github.com/matt-riley/panda/commit/83299c600991882169e2a1c8b361406f9b20dfdf))
* add paginated ls to plan mode and Codemode ([#34](https://github.com/matt-riley/panda/issues/34)) ([8b95253](https://github.com/matt-riley/panda/commit/8b9525388f87921d6bcb1c555255e837842ab725))
* honor .gitignore in search and repo_map ([#32](https://github.com/matt-riley/panda/issues/32)) ([3f22dbf](https://github.com/matt-riley/panda/commit/3f22dbfcfb79141c4b5205074ad0907be7154eb5))
* support regex and glob search in plan mode and Codemode ([#31](https://github.com/matt-riley/panda/issues/31)) ([c3eec36](https://github.com/matt-riley/panda/commit/c3eec36537e278dfc13fc6c3e883b915aad92535))
* support reproducible one-shot CLI runs ([#35](https://github.com/matt-riley/panda/issues/35)) ([c4fed94](https://github.com/matt-riley/panda/commit/c4fed944ea45b7fe2ca8dfb5e058af714398c485))


### Bug Fixes

* guide tool selection for named files and paginated searches ([#29](https://github.com/matt-riley/panda/issues/29)) ([c2b2877](https://github.com/matt-riley/panda/commit/c2b2877fbf47b517816a1b5b8cd14e56e523043b))

## [0.1.13](https://github.com/matt-riley/panda/compare/v0.1.12...v0.1.13) (2026-11-19)


### Features

* add opt-in read-only Codemode ([#27](https://github.com/matt-riley/panda/issues/27)) ([24ee26f](https://github.com/matt-riley/panda/commit/24ee26fa1f30d70fa1f6235706b6aadf368583fc))

## [0.1.12](https://github.com/matt-riley/panda/compare/v0.1.11...v0.1.12) (2026-11-18)


### Features

* **core:** recover transient requests and compact against the model window ([#25](https://github.com/matt-riley/panda/issues/25)) ([6ca88f8](https://github.com/matt-riley/panda/commit/6ca88f8f06821969ab013b7ad6fb82ed5c053f9c))

## [0.1.11](https://github.com/matt-riley/panda/compare/v0.1.10...v0.1.11) (2026-11-18)


### Features

* **cli:** copy the last reply on the active branch ([#23](https://github.com/matt-riley/panda/issues/23)) ([7b88ecf](https://github.com/matt-riley/panda/commit/7b88ecf50323682fc3e57f667da75c847f67feb5))

## [0.1.10](https://github.com/matt-riley/panda/compare/v0.1.9...v0.1.10) (2026-11-18)


### Features

* **cli:** summarize a branch when switching in the session tree ([#21](https://github.com/matt-riley/panda/issues/21)) ([fa566f1](https://github.com/matt-riley/panda/commit/fa566f12aa7717718a2f543d49e1680e35cf3d98))

## [0.1.9](https://github.com/matt-riley/panda/compare/v0.1.8...v0.1.9) (2026-11-18)


### Features

* **cli:** navigate and branch within a session tree ([#19](https://github.com/matt-riley/panda/issues/19)) ([6b42d1d](https://github.com/matt-riley/panda/commit/6b42d1d01136e0a50ecc51bf8b6ccbc5a44e82ac))

## [0.1.8](https://github.com/matt-riley/panda/compare/v0.1.7...v0.1.8) (2026-11-18)


### Features

* **cli:** search and preview sessions in the resume picker ([#17](https://github.com/matt-riley/panda/issues/17)) ([951b759](https://github.com/matt-riley/panda/commit/951b759939611495e1fb69e293d551a2b6b27db7))

## [0.1.7](https://github.com/matt-riley/panda/compare/v0.1.6...v0.1.7) (2026-11-18)


### Features

* **cli:** complete Normal-mode commands in both prompts ([#15](https://github.com/matt-riley/panda/issues/15)) ([f7426f3](https://github.com/matt-riley/panda/commit/f7426f317cc5aea1395a2cbc355554be1a1b6c82))

## [0.1.6](https://github.com/matt-riley/panda/compare/v0.1.5...v0.1.6) (2026-11-18)


### Features

* **cli:** edit steering prompts with Vim and run colon commands ([8e82927](https://github.com/matt-riley/panda/commit/8e829277512f07da9ff9aa37620f1046ed479d87))

## [0.1.5](https://github.com/matt-riley/panda/compare/v0.1.4...v0.1.5) (2026-11-18)


### Features

* **cli:** render streaming and resumed assistant replies as Markdown ([06acb03](https://github.com/matt-riley/panda/commit/06acb033fe4659b2a090ac020e85d2e425c3d090))
* render streaming Markdown with live and resume parity ([5618558](https://github.com/matt-riley/panda/commit/5618558ce00aca6e23182630f5394570a4c6db0b))

## [0.1.4](https://github.com/matt-riley/panda/compare/v0.1.3...v0.1.4) (2026-11-18)


### Features

* **cli:** render active turns and reasoning effort pickers with Ratatui ([c106e54](https://github.com/matt-riley/panda/commit/c106e5431579276bb191fb3348ba2f97b2c23359))
* **providers:** expose model reasoning efforts from the Codex catalogue ([a2cd3d6](https://github.com/matt-riley/panda/commit/a2cd3d631e0b539f1e48721eb541bb69234d38e9))
* render active turns and effort selection with Ratatui ([bf89d5f](https://github.com/matt-riley/panda/commit/bf89d5f8db885375e0416e0c6925e22c0a6bc10a))

## [0.1.3](https://github.com/matt-riley/panda/compare/v0.1.2...v0.1.3) (2026-11-18)


### Bug Fixes

* **cli:** make login and model selection searchable in the terminal ([ad4ab16](https://github.com/matt-riley/panda/commit/ad4ab16abea0483dddae8b91e6fe8ad90adf44ef))
* **cli:** preserve oversized graphemes when wrapping terminal output ([76e4291](https://github.com/matt-riley/panda/commit/76e42918b54852d5af91f94c62eb0f8c1109956e))
* **providers:** discover current Codex models and open browser sign-in ([04c9b2c](https://github.com/matt-riley/panda/commit/04c9b2c53a4bd29e733d58909ed39baaf2620ba3))
* restore Codex login and model selection in the terminal ([c602377](https://github.com/matt-riley/panda/commit/c6023770ebe325e45d563eb6d54d8b74a1f5759d))

## [0.1.2](https://github.com/matt-riley/panda/compare/v0.1.1...v0.1.2) (2026-11-18)


### Features

* add full-screen coding sessions and scripting interfaces ([5326d2e](https://github.com/matt-riley/panda/commit/5326d2eed103ea8aa30ff5eb9b8eee8474904684))
* **cli:** add the full-screen coding terminal and scripting modes ([4a7b144](https://github.com/matt-riley/panda/commit/4a7b14470f287055a4e13a52b104685a80424bd7))
* **coding:** stream workspace tools and read scoped instructions ([b706ed7](https://github.com/matt-riley/panda/commit/b706ed7e104828af9ffa13108662e04f7bb340bc))
* **core:** steer and compact sessions with multimodal provider support ([f33811f](https://github.com/matt-riley/panda/commit/f33811fc0dd45ec1121331317c2ea48cb27c0c2f))
* **local:** rank Markdown skills by name and description ([b8ee232](https://github.com/matt-riley/panda/commit/b8ee232372436edd10b598b3f7eae303dde4da92))
* **local:** support session forks and named history ([052e308](https://github.com/matt-riley/panda/commit/052e308a5b9d8bf2c3444b6664542efc9e96bca7))

## [0.1.1](https://github.com/matt-riley/panda/compare/v0.1.0...v0.1.1) (2026-11-18)


### Features

* **cli:** add a Vim prompt editor, session status and compact tool output ([7beed22](https://github.com/matt-riley/panda/commit/7beed224b327910b0309300358055a0f23b8d407))

## 0.1.0 (2026-11-18)


### Features

* ship the Panda coding harness for macOS ([08ff041](https://github.com/matt-riley/panda/commit/08ff04189e62a5f5d790311c03ca98d75d5ba654))
