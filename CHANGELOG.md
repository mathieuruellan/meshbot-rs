# Changelog

## [0.4.1](https://github.com/mathieuruellan/meshbot-rs/compare/v0.4.0...v0.4.1) (2026-10-02)


### Bug Fixes

* green the checks job for the command marker ([0b8d40b](https://github.com/mathieuruellan/meshbot-rs/commit/0b8d40b06b9a9650a7b12a8ffd9dde830593a5a0))
* make the radio channel table strictly read-only ([8646cf0](https://github.com/mathieuruellan/meshbot-rs/commit/8646cf08a760cc1eba8ac79ad4424de22622df01))
* parse command without nickname prefix ([ed6bb64](https://github.com/mathieuruellan/meshbot-rs/commit/ed6bb6441ef1291affaf9adecb8fe02157c670e8))
* require the ! command marker ([79f3e07](https://github.com/mathieuruellan/meshbot-rs/commit/79f3e07ecf5f6594f6e517890294d1d633d30949))
* serialise the script tests and make a failed fork visible ([3f5aa9c](https://github.com/mathieuruellan/meshbot-rs/commit/3f5aa9c42cacfad39980a767e35da3e142585dda))

## [0.4.0](https://github.com/mathieuruellan/meshbot-rs/compare/v0.3.0...v0.4.0) (2026-10-01)


### Features

* multi-line action replies; fix(scripts): komodo uses key+secret and real endpoints ([fbea379](https://github.com/mathieuruellan/meshbot-rs/commit/fbea3797e20af160ca5e1752a88b5d125d909f49))
* multi-line action replies; fix(scripts): komodo uses key+secret… ([9199c9c](https://github.com/mathieuruellan/meshbot-rs/commit/9199c9c10b9405013530f9a3e24a58ce4faf6a3a))

## [0.3.0](https://github.com/mathieuruellan/meshbot-rs/compare/v0.2.0...v0.3.0) (2026-10-01)


### Features

* **config:** read the verb table from config.yaml ([9e298a6](https://github.com/mathieuruellan/meshbot-rs/commit/9e298a6cbd3e8a7373e9485f013bbf8ff35a8a9a))
* own the radio clock ([d6bcd11](https://github.com/mathieuruellan/meshbot-rs/commit/d6bcd111018a6805c0c33756f2ad1ef7953e0781))


### Bug Fixes

* answer an unknown verb only when it is near a real one ([e2f3537](https://github.com/mathieuruellan/meshbot-rs/commit/e2f35379b38191a2b5be004a25740c4090fdba5e))
* **ci:** build the released image from the main push, not the tag push ([74a3bd2](https://github.com/mathieuruellan/meshbot-rs/commit/74a3bd2f0a7a59c132a6c53c617cb423ca150368))

## [0.2.0](https://github.com/mathieuruellan/meshbot-rs/compare/v0.1.0...v0.2.0) (2026-09-30)


### Features

* build and publish a container image, versioned from commits ([94cfeda](https://github.com/mathieuruellan/meshbot-rs/commit/94cfedacd986f02cfcc88410457aaedc8ddccfd1))


### Bug Fixes

* **ci:** pass GITHUB_TOKEN to semantic-pull-request ([dfee613](https://github.com/mathieuruellan/meshbot-rs/commit/dfee613ba143648adea1e8f9c84368f2d3c9a132))
