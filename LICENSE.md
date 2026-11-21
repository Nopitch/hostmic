# Licenses

hostmic is distributed under more than one license: each crate states its own in its
`Cargo.toml` (SPDX expression), and the license texts are in [`LICENSES/`](LICENSES), named by
their SPDX identifiers, as in the [REUSE](https://reuse.software/) layout.

| Crate | Directory | License |
|---|---|---|
| `hostmic-driver`, the kernel driver | `driver/` | [MS-PL](LICENSES/MS-PL.txt) |
| `hostmic-driver-core`, its pure logic | `driver-core/` | [MS-PL](LICENSES/MS-PL.txt) |
| `hostmic`, the host library | `host/` | [MIT](LICENSES/MIT.txt) OR [Apache-2.0](LICENSES/Apache-2.0.txt) |
| `hostmic-abi`, the driver–host contract | `abi/` | [MIT](LICENSES/MIT.txt) OR [Apache-2.0](LICENSES/Apache-2.0.txt) |
| `hostmic-sys`, the generated bindings | `hostmic-sys/` | [MIT](LICENSES/MIT.txt) OR [Apache-2.0](LICENSES/Apache-2.0.txt) |
| the tools (`capture-check`, `handle-check`, `hostmic-ctl`, `ioctl-fuzz`, `measure`, `test-host`) | `tools/` | [MIT](LICENSES/MIT.txt) OR [Apache-2.0](LICENSES/Apache-2.0.txt) |
| the fuzz targets | `fuzz/` | [MIT](LICENSES/MIT.txt) OR [Apache-2.0](LICENSES/Apache-2.0.txt) |

The driver is a port of Microsoft's audio driver samples and keeps their license, the Microsoft
Public License. Everything a host links against (the `hostmic` library and `hostmic-abi`, the
contract it shares with the driver) is MIT OR Apache-2.0, at the user's option. Why each
license applies is explained in the README's [Licensing](README.md#licensing) section;
contributions follow the license of the crate they belong to
([CONTRIBUTING.md](CONTRIBUTING.md#licensing-of-contributions)).

GitHub labels the repository's license "Other" on purpose. Its detector reads only license
files at the root and reports a single license, so root copies of the texts would show one
license for the whole repository and misstate the MS-PL driver. The table above and each
crate's `Cargo.toml` are the authoritative statement.
