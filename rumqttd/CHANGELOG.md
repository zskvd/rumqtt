# CHANGELOG

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `PrometheusSetting` now takes `listen` to specify listener address instead of default `127.0.0.1`. Do not use `listen` and `port` together.

### Changed

### Deprecated
- `PrometheusSetting`'s `port` will be removed in favour of `listen`.

### Removed

### Fixed

### Security

---

## [rumqttd 0.13.0] - 08-03-2023

- No change, only version bump

## [rumqttd 0.12.7] - 04-03-2023

### Changed
- Re-design meters and alerts (#579)

## [rumqttd 0.12.6] - 14-02-2023

### Added
- Print version number with `--version` cli flag (#578)

## [rumqttd 0.12.5] - 07-02-2023

### Changed
- `structopt` is in maintainance mode, we now use `clap` instead (#571)

### Fixed
- Use `error` instead of `debug` to log message related to duplicate client_id (#572)

## [rumqttd 0.12.4] - 01-02-2023

### Fixed
- Client id with tenant prefix should be set globally (#564) 

## [rumqttd 0.12.3] - 23-01-2023

### Added
- Add one way bridging support via BridgeLink (#558)

### Changed
- Restructure AlertsLink and MetersLink to support writing to Clickhouse (#557)

## [rumqttd 0.12.2] - 16-01-2023

### Added
- Add AlertLink to get alerts about router events (#538)
- Add basic username and password authentication (#553)

### Changed
- Don't allow client's with empty client_id (#546)
- Disconnect existing client on a new connection with similar client_id (#546)
- Skip client certificate verification when using native-tls because it doesn't support it (#550)


---

Old changelog entries can be found at [CHANGELOG.md](../CHANGELOG.md)
