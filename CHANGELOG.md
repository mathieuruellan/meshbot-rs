# Changelog

## [0.8.1](https://github.com/mathieuruellan/meshbot-rs/compare/v0.8.0...v0.8.1) (2026-10-08)


### Bug Fixes

* limit help to the channel's verbs and drop typo suggestions ([dd8ec23](https://github.com/mathieuruellan/meshbot-rs/commit/dd8ec23b7b06a2e3a87fdd1ab5964711c286675b))
* limit help to the channel's verbs and drop typo suggestions ([4224895](https://github.com/mathieuruellan/meshbot-rs/commit/42248951795dd69beee89dd46a84b458cd367a1c))

## [0.8.0](https://github.com/mathieuruellan/meshbot-rs/compare/v0.7.1...v0.8.0) (2026-10-05)


### Features

* **webhook:** address a channel by index or by name ([bc1deda](https://github.com/mathieuruellan/meshbot-rs/commit/bc1deda0dbd469b8f593f15977536eb6954c7065))
* **webhook:** HTTP endpoint to post on a monitored channel ([31cf0f8](https://github.com/mathieuruellan/meshbot-rs/commit/31cf0f87900f0356137c7c173431efb01738ef4d))
* **webhook:** HTTP endpoint to post on a monitored channel ([6eb1230](https://github.com/mathieuruellan/meshbot-rs/commit/6eb1230af3dba9ea62268020f84579a3f2a9c4ca))

## [0.7.1](https://github.com/mathieuruellan/meshbot-rs/compare/v0.7.0...v0.7.1) (2026-10-04)


### Bug Fixes

* adopt rust 1.99 toolchain and drop deprecated fetch_update ([1b50bc9](https://github.com/mathieuruellan/meshbot-rs/commit/1b50bc9f11b803e354f3a167c0bde2a7b52bae48))
* **ci:** run renovate for real instead of as a dry run ([b4155c3](https://github.com/mathieuruellan/meshbot-rs/commit/b4155c30bf9c7da67cef90ff1cd8005f48754401))
* keep hop names resolving after another client polls contacts ([18d312c](https://github.com/mathieuruellan/meshbot-rs/commit/18d312c220809277621c89539b7035520ebcceab))
* keep hop names resolving after another client polls contacts ([fe43e22](https://github.com/mathieuruellan/meshbot-rs/commit/fe43e229e6dc3d17ec500f5ac79d6887b10cc59a))

## [0.7.0](https://github.com/mathieuruellan/meshbot-rs/compare/v0.6.1...v0.7.0) (2026-10-03)


### Features

* accept multi-word sender tags and name the sender in replies ([62e9724](https://github.com/mathieuruellan/meshbot-rs/commit/62e972494fc2fd4b5cd1e7c2d60caabbed479753))
* accept multi-word sender tags and name the sender in replies ([b9aea07](https://github.com/mathieuruellan/meshbot-rs/commit/b9aea073bc357b506724660961eac2e86c002b0c))

## [0.6.1](https://github.com/mathieuruellan/meshbot-rs/compare/v0.6.0...v0.6.1) (2026-10-03)


### Bug Fixes

* decode the packed path byte so hops is a hop count ([deb60f5](https://github.com/mathieuruellan/meshbot-rs/commit/deb60f53e54e71c311cc9a60fd6c5b7193fead16))
* decode the packed path byte so hops is a hop count ([7cd9e99](https://github.com/mathieuruellan/meshbot-rs/commit/7cd9e99afc1b10b55e3692d81c4522e42a947c84))

## [0.6.0](https://github.com/mathieuruellan/meshbot-rs/compare/v0.5.0...v0.6.0) (2026-10-03)


### Features

* report channel message path, hops and delay ([0c3e65b](https://github.com/mathieuruellan/meshbot-rs/commit/0c3e65bd98a976f68407981bfa59a8c2305e2d52))
* report channel message path, hops and delay ([efb021d](https://github.com/mathieuruellan/meshbot-rs/commit/efb021d390b0e6d1c2253d4b75caecbd93311295))

## [0.5.0](https://github.com/mathieuruellan/meshbot-rs/compare/v0.4.1...v0.5.0) (2026-10-02)


### Features

* build and publish a container image, versioned from commits ([94cfeda](https://github.com/mathieuruellan/meshbot-rs/commit/94cfedacd986f02cfcc88410457aaedc8ddccfd1))
* **config:** read the verb table from config.yaml ([9e298a6](https://github.com/mathieuruellan/meshbot-rs/commit/9e298a6cbd3e8a7373e9485f013bbf8ff35a8a9a))
* multi-line action replies; fix(scripts): komodo uses key+secret and real endpoints ([fbea379](https://github.com/mathieuruellan/meshbot-rs/commit/fbea3797e20af160ca5e1752a88b5d125d909f49))
* multi-line action replies; fix(scripts): komodo uses key+secret… ([9199c9c](https://github.com/mathieuruellan/meshbot-rs/commit/9199c9c10b9405013530f9a3e24a58ce4faf6a3a))
* own the radio clock ([d6bcd11](https://github.com/mathieuruellan/meshbot-rs/commit/d6bcd111018a6805c0c33756f2ad1ef7953e0781))


### Bug Fixes

* answer an unknown verb only when it is near a real one ([e2f3537](https://github.com/mathieuruellan/meshbot-rs/commit/e2f35379b38191a2b5be004a25740c4090fdba5e))
* **ci:** build the released image from the main push, not the tag push ([74a3bd2](https://github.com/mathieuruellan/meshbot-rs/commit/74a3bd2f0a7a59c132a6c53c617cb423ca150368))
* **ci:** pass GITHUB_TOKEN to semantic-pull-request ([dfee613](https://github.com/mathieuruellan/meshbot-rs/commit/dfee613ba143648adea1e8f9c84368f2d3c9a132))
* green the checks job for the command marker ([0b8d40b](https://github.com/mathieuruellan/meshbot-rs/commit/0b8d40b06b9a9650a7b12a8ffd9dde830593a5a0))
* make the radio channel table strictly read-only ([8646cf0](https://github.com/mathieuruellan/meshbot-rs/commit/8646cf08a760cc1eba8ac79ad4424de22622df01))
* parse command without nickname prefix ([ed6bb64](https://github.com/mathieuruellan/meshbot-rs/commit/ed6bb6441ef1291affaf9adecb8fe02157c670e8))
* reconnect and re-sync the radio clock after a power-cycle ([ffc488a](https://github.com/mathieuruellan/meshbot-rs/commit/ffc488af644a61f16961fe7b1e510b1c933f1bab))
* reconnect and re-sync the radio clock after a power-cycle ([bc7eb5d](https://github.com/mathieuruellan/meshbot-rs/commit/bc7eb5db251cd9a779efaf7a689796a5e5a444d9))
* require the ! command marker ([79f3e07](https://github.com/mathieuruellan/meshbot-rs/commit/79f3e07ecf5f6594f6e517890294d1d633d30949))
* serialise the script tests and make a failed fork visible ([3f5aa9c](https://github.com/mathieuruellan/meshbot-rs/commit/3f5aa9c42cacfad39980a767e35da3e142585dda))

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
