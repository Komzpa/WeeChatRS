# egui-winit

[![Latest version](https://img.shields.io/crates/v/egui-winit.svg)](https://crates.io/crates/egui-winit)
[![Documentation](https://docs.rs/egui-winit/badge.svg)](https://docs.rs/egui-winit)
![MIT](https://img.shields.io/badge/license-MIT-blue.svg)
![Apache](https://img.shields.io/badge/license-Apache-blue.svg)

This crates provides bindings between [`egui`](https://github.com/emilk/egui) and [`winit`](https://crates.io/crates/winit).

The library translates winit events to egui, handled copy/paste, updates the cursor, open links clicked in egui, etc.

## WeeChatRS patch provenance

This directory vendors the published `egui-winit` 0.27.2 crate from the
`emilk/egui` repository (crate VCS revision
`014327e36535deeca0839b6aca2646191f2bd2fb`). WeeChatRS changes only the key
event translation in `src/lib.rs`: command shortcuts fall back to the physical
letter on non-Latin layouts, and paste still emits the key event needed for
native image clipboard handling. The parent `vendor/LICENSE-APACHE` and
`vendor/LICENSE-MIT` files retain the upstream dual-license terms.
