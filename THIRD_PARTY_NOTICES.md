# Third-party notices

| Component | Use | License |
|---|---|---|
| [IronRDP](https://github.com/Devolutions/IronRDP) (`ironrdp-*` crates, including RDPEUDP2/HSRP, RDPEMT and multitransport) | RDP protocol core | Apache-2.0 OR MIT |
| [IronRDP](https://github.com/Devolutions/IronRDP) `ironrdp-server` 0.14.0, pinned revision `475e5d534f0afa080e51b940caf3d7c1f9d6ec6c` | Local Cargo patch adds opt-in, metadata-only logging for the first legacy bitmap update, the Android-compatible byte count for RDP 6.0 32-bpp `cbScanWidth`, and an observer fired only after EGFX output has migrated to reliable UDP | Apache-2.0 OR MIT; license texts retained in `core/vendor/ironrdp-server-0.14-diagnostics/` |
| IronRDP `ironrdp-server` 0.13.0 and `ironrdp-egfx` 0.3.0, retained in `core/vendor/` as historical patched source (not part of the current Cargo dependency graph; ADR-017) | RDP server and EGFX protocol | Apache-2.0 OR MIT |
| [cbindgen](https://github.com/mozilla/cbindgen) | Build-time C header generation | MPL-2.0 (build tool only; not distributed) |
| [DeskPad](https://github.com/Stengo/DeskPad) | Reference for the shape of the private `CGVirtualDisplay` ObjC interface (`spikes/virtual-display/CGVirtualDisplay.h`) | MIT |
| [Contributor Covenant 2.1](https://www.contributor-covenant.org) | `CODE_OF_CONDUCT.md` | CC BY 4.0 |

Transitive Rust dependency licenses will be generated with `cargo about` before the first release.
