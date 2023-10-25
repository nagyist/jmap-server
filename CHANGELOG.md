# Change Log

All notable changes to this project will be documented in this file. This project adheres to [Semantic Versioning](http://semver.org/).

## [0.4.0] - 2023-10-25

This version introduces some breaking changes in the configuration file. Please read the [UPGRADING.md](UPGRADING.md) file for more information.

## Added
- Built-in Spam and Phishing filter.
- Scheduled queries on some directory types.
- In-memory maps and lists containing glob or regex patterns.
- Remote retrieval of in-memory list/maps with fallback mechanisms.
- Macros and support for including files from TOML config files.

### Changed
- `config.toml` is now split in multiple TOML files for better organization.
- **BREAKING:** Configuration key prefix `jmap.sieve` (JMAP Sieve Interpreter) has been renamed to `sieve.untrusted`.
- **BREAKING:** Configuration key prefix `sieve` (SMTP Sieve Interpreter) has been renamed to `sieve.trusted`.

### Fixed

## [0.3.8] - 2023-09-19

## Added
- Journal logging support

### Changed
- Replaced `rpgp` with `sequoia-pgp` due to rpgp bug.

### Fixed

## [0.3.7] - 2023-09-05

## Added
- Option to allow unencrypted SMTP AUTH (#72)
- Support for `rcpt-domain` key in `rcpt.relay` SMTP rule evaluation.

### Changed
 
### Fixed
- SMTP strategy `Ipv6thenIpv4` returns only IPv6 addresses (#70)
- Milter `DATA` command is sent after headers which causes ClamAV to hang.
- Sieve `redirect` of unmodified messages does not work.

## [0.3.6] - 2023-08-29

## Added
- Arithmetic and logical expression evaluation in Sieve scripts.
- Support for storing query results in Sieve variables.
- Results of SPF, DKIM, ARC, DMARC and IPREV checks available as environment variables in Sieve scripts.
- Configurable protocol flags for Milter filters.
- Fall-back to plain text when `STARTTLS` fails and `starttls` is set to `optional`.

### Changed
 
### Fixed
- Do not panic when `hash = 0` in reports. (#60)
- JMAP Session resource returns `EmailSubmission` capabilities using arrays rather than objects.
- ManageSieve `PUTSCRIPT` should replace existing scripts.

## [0.3.5] - 2023-08-18

## Added
- TCP listener option `nodelay`.
 
### Changed
 
### Fixed
- SMTP: Allow disabling `STARTTLS`.
- JMAP: Support for `OPTIONS` HTTP method.

## [0.3.4] - 2023-08-09

## Added
- JMAP: Support for setting custom HTTP response headers (#52)
 
### Changed
 
### Fixed
- JMAP/IMAP: Successful authentication requests should not count when rate limiting
- IMAP: Case insensitive Inbox selection
- IMAP: Automatically create Inbox for group accounts

## [0.3.3] - 2023-08-02

### Added
- Encryption at rest with **S/MIME** or **OpenPGP**.
- Support for referencing context variables from dynamic values.
 
### Changed
 
### Fixed
- Support for PKCS8v1 ED25519 keys (#20).
- Automatic retry for import/export blob downloads (#14)

## [0.3.2] - 2023-07-28

### Added
- Sender and recipient address rewriting using regular expressions and sieve scripts.
- Subaddressing and catch-all addresses using regular expressions (#10).
- Dynamic variables in SMTP rules.
 
### Changed
- Added CLI to Docker container (#19).
 
### Fixed
- Workaround for a bug in `sqlx` that caused SQL time-outs (#15).
- Support for ED25519 certificates in PEM files (#20). 
- Better handling of concurrent IMAP UID map modifications (#17).
- LDAP domain lookups from SMTP rules.

## [0.3.1] - 2023-07-22

### Added
- Milter filter support.
- Match IP address type using /0 mask (#16).
 
### Changed
 
### Fixed
- Support for OpenLDAP password hashing schemes between curly brackets (#8). 
- Add CA certificates to Docker runtime (#5).

## [0.3.0] - 2023-07-16

### Added
- **LDAP** and **SQL** authentication.
- **subaddressing** and **catch-all** addresses.
- **S3-compatible** storage.

### Changed
- Merged the `stalwart-jmap`, `stalwart-imap` and `stalwart-smtp` repositories into
  `stalwart-mail`.
- Removed clustering module and replaced it with a **FoundationDB** backend option.
- Integrated Stalwart SMTP into Stalwart JMAP.
- Rewritten JMAP protocol parser.
- Rewritten store backend.
- Rewritten IMAP server to have direct access to the message store (no more IMAP proxy).
- Replaced `actix` with `hyper`.
 
### Fixed

## [0.2.0] - 2022-10-31

### Added
- JMAP for Sieve support.

### Changed
 
### Fixed
- Faster parsing of e-mail messages.

## [0.1.0] - 2022-09-17

Initial release.

