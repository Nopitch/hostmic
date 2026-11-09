# Licenses

hostmic is distributed under more than one license: each crate states its own in its
`Cargo.toml` (SPDX expression), and the license texts are in [`LICENSES/`](LICENSES), named by
their SPDX identifiers, as in the [REUSE](https://reuse.software/) layout.

| Crate | Directory | License |
|---|---|---|
| `hostmic-driver`, the kernel driver | `driver/` | [MS-PL](LICENSES/MS-PL.txt) |
| `hostmic-driver-core`, its pure logic | `driver-core/` | [MS-PL](LICENSES/MS-PL.txt) |
| `hostmic-abi`, the driver–host contract | `abi/` | [MIT](LICENSES/MIT.txt) OR [Apache-2.0](LICENSES/Apache-2.0.txt) |
| `hostmic-sys`, the generated bindings | `hostmic-sys/` | [MIT](LICENSES/MIT.txt) OR [Apache-2.0](LICENSES/Apache-2.0.txt) |
| the tools (`capture-check`, `hostmic-ctl`, `test-host`) | `tools/` | [MIT](LICENSES/MIT.txt) OR [Apache-2.0](LICENSES/Apache-2.0.txt) |
| the fuzz targets | `fuzz/` | [MIT](LICENSES/MIT.txt) OR [Apache-2.0](LICENSES/Apache-2.0.txt) |

The driver is a port of Microsoft's audio driver samples and keeps their license, the Microsoft
Public License. Everything a host links against (`hostmic-abi`, and the host library to come)
is MIT OR Apache-2.0, at the user's option. Why each license applies is explained in the
README's [Licensing](README.md#licensing) section; contributions follow the license of the
crate they belong to ([CONTRIBUTING.md](CONTRIBUTING.md#licensing-of-contributions)).
