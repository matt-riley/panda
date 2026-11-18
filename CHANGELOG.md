# Changelog

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
