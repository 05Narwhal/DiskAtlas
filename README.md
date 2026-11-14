# Disk Atlas

**A fast, extensible, cross-platform filesystem explorer built with Rust.**

[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg)](LICENSE)
[![Language: Rust](https://img.shields.io/badge/Language-Rust-orange.svg)](https://www.rust-lang.org/)
[![Status: Alpha](https://img.shields.io/badge/Status-Alpha-yellow.svg)](#project-status)

Disk Atlas is an open-source filesystem explorer focused on performance, extensibility, and a modern desktop experience.

The project aims to make exploring unfamiliar and platform-specific filesystems straightforward, with particular attention to Apple filesystems and storage devices that are not natively accessible on every operating system.

Built with Rust, Disk Atlas is designed around a modular architecture that separates its user interface from its filesystem engine, allowing its core functionality to be reused independently.

> **Project status:** Disk Atlas is currently in early development. Features described as planned or intended are not necessarily implemented yet.

---

## Table of Contents

- [Disk Atlas](#disk-atlas)
  - [Table of Contents](#table-of-contents)
  - [Overview](#overview)
  - [Goals](#goals)
  - [Planned Features](#planned-features)
    - [Filesystem browsing](#filesystem-browsing)
    - [File previews](#file-previews)
    - [Search and indexing](#search-and-indexing)
    - [Modern desktop interface](#modern-desktop-interface)
    - [Plugin system](#plugin-system)
  - [Filesystem Support](#filesystem-support)
  - [Architecture](#architecture)
    - [Core components](#core-components)
  - [Technology Stack](#technology-stack)
  - [Project Structure](#project-structure)
    - [Crates](#crates)
  - [Getting Started](#getting-started)
    - [Requirements](#requirements)
    - [Clone the repository](#clone-the-repository)
    - [Build the project](#build-the-project)
    - [Run the application](#run-the-application)
    - [Check the workspace](#check-the-workspace)
    - [Run tests](#run-tests)
  - [Development](#development)
    - [Development principles](#development-principles)
    - [Formatting](#formatting)
    - [Linting](#linting)
    - [Testing](#testing)
  - [Roadmap](#roadmap)
  - [Contributing](#contributing)
    - [How to contribute](#how-to-contribute)
  - [License](#license)
  - [Acknowledgements](#acknowledgements)
  - [Contact](#contact)

---

## Overview

Accessing filesystems created by other operating systems can be unnecessarily difficult.

For example, accessing an Apple-formatted drive from a Windows or Linux computer may require specialized software, often with limited functionality or outdated interfaces.

Disk Atlas aims to address this by providing a unified desktop application for browsing, inspecting, and eventually managing files across different filesystem formats.

The project prioritizes:

- **Performance:** Efficient filesystem parsing, directory traversal, indexing, and file previews.
- **Cross-platform support:** A consistent experience across Windows, Linux, and macOS.
- **Extensibility:** A modular architecture that can accommodate additional filesystem implementations and third-party extensions.
- **Usability:** A modern, keyboard-friendly interface with intuitive navigation.
- **Data integrity:** Careful handling of filesystem metadata and a read-only-first development approach.

## Goals

The long-term goal is to create a capable alternative to traditional filesystem browsing utilities, particularly when working with disks formatted for other operating systems.

Disk Atlas is intended to be:

- A desktop filesystem explorer, not just a filesystem extraction utility.
- Capable of accessing filesystems that the host operating system does not natively support.
- Fast enough to handle large directory structures and substantial storage devices.
- Extensible through a dedicated plugin architecture.
- Independent of its graphical interface at the filesystem-engine level.
- Free and open source.

## Planned Features

The following describes the intended feature set. Implementation status will evolve throughout development.

### Filesystem browsing

- Browse disks, partitions, volumes, and directories.
- Navigate directory structures efficiently.
- Inspect file properties and metadata.
- Support different filesystem formats through modular implementations.
- Handle large directory structures without unnecessarily loading everything into memory.

### File previews

A built-in preview system is planned, with support for:

- Images.
- Text files and source code.
- PDF documents.
- Audio and video.
- Archives.
- Binary files and hexadecimal inspection.

A Quick Look-inspired preview experience is also planned, allowing users to select a file and press `Space` to inspect it without opening another application.

### Search and indexing

- Fast filename and path searches.
- Optional filesystem indexing.
- Filtering by file type, size, and other metadata.
- Efficient handling of large volumes.

### Modern desktop interface

- Native desktop application built with Rust.
- Sidebar navigation for disks, volumes, and bookmarks.
- Breadcrumb-based directory navigation.
- List and grid views.
- Optional file preview panel.
- Keyboard-first navigation.
- Themes and customizable appearance.
- Smooth transitions and interface animations.

### Plugin system

Disk Atlas is intended to support third-party extensions.

Potential extension categories include:

- Filesystem providers.
- File preview providers.
- Search providers.
- Metadata extractors.
- File format handlers.
- Custom commands and actions.

The plugin architecture is intended to provide a stable interface without requiring plugins to be compiled directly into the application.

---

## Filesystem Support

Apple filesystem compatibility is one of the project's primary objectives.

The planned development priorities are:

| Filesystem | Priority | Status |
|---|---|---|
| HFS | Planned | Not implemented |
| HFS+ | Planned | Not implemented |
| HFSX | Planned | Not implemented |
| APFS | Long-term goal | Not implemented |
| Encrypted APFS | Long-term goal | Not implemented |

Filesystem support will be developed incrementally.

The intention is to prioritize reliable read access and metadata preservation before introducing potentially destructive operations.

**Important:** The table represents development intentions, not currently supported formats. Do not rely on Disk Atlas for accessing or recovering important data until the relevant functionality has been implemented and tested.

---

## Architecture

Disk Atlas is organized as a Rust workspace with separate application and library crates.

The graphical application is responsible for the user experience, while the library contains the underlying filesystem functionality.

This separation is intentional: the filesystem engine should remain usable independently of the graphical application.

### Core components

| Component | Responsibility |
|---|---|
| Application | Desktop interface and user interaction |
| Filesystem library | Filesystem parsing and directory operations |
| Preview engine | File inspection and preview generation |
| Search engine | Searching and indexing |
| Plugin runtime | Loading and managing extensions |

Additional components may be introduced as the project develops.

---

## Technology Stack

| Technology | Purpose |
|---|---|
| Rust | Primary programming language |
| GPUI | Desktop user interface |
| Serde | Serialization and deserialization |
| Rayon | Parallel processing, where needed |
| Tracing | Logging and diagnostics |
| Anyhow | Application-level error handling |
| Thiserror | Structured library errors |

The technology stack may evolve as development progresses.

---

## Project Structure

```text
disk-atlas/
├── .github/
│   └── workflows/          CI: fmt, clippy and tests on Windows, Linux and macOS
│
├── assets/
│   ├── fonts/
│   └── icons/
│
├── crates/
│   ├── app/                disk-atlas: the desktop application
│   │   ├── src/
│   │   │   ├── main.rs
│   │   │   ├── app.rs
│   │   │   ├── theme/
│   │   │   ├── components/
│   │   │   └── views/
│   │   └── Cargo.toml
│   │
│   └── lib/                disk-atlas-lib: the filesystem engine
│       ├── src/
│       │   ├── lib.rs
│       │   ├── error.rs
│       │   ├── io/         block devices and caching
│       │   ├── image/      disk image formats
│       │   ├── partition/  partition maps
│       │   ├── fs/         filesystems
│       │   ├── search/     name index and queries
│       │   └── extract/    copying files out
│       └── Cargo.toml
│
├── .gitignore
├── Cargo.lock
├── Cargo.toml
├── CONTRIBUTORS.md
├── LICENSE
└── README.md
```

### Crates

**`crates/app` — Desktop application**

Contains the executable application, graphical interface, and application-level logic.

**`crates/lib` — Filesystem library**

Contains the reusable filesystem engine and related functionality. It is designed to remain independent of the graphical interface.

---

## Getting Started

### Requirements

- A supported operating system.
- [Rust](https://www.rust-lang.org/tools/install) and Cargo.
- Git.
- Platform-specific dependencies required by GPUI.

### Clone the repository

```bash
git clone https://github.com/05Narwhal/DiskAtlas.git
cd DiskAtlas
```

### Build the project

```bash
cargo build --workspace
```

### Run the application

```bash
cargo run -p disk-atlas
```

### Check the workspace

```bash
cargo check --workspace
```

### Run tests

```bash
cargo test --workspace
```

**Note:** These commands assume the relevant crates and application entry point are in place. Build requirements may change as the project develops.

---

## Development

Disk Atlas is being developed with modularity and maintainability in mind.

When implementing new functionality, the preferred approach is to keep filesystem-specific logic inside the library rather than coupling it directly to the graphical interface.

### Development principles

1. Keep filesystem operations separate from UI logic.
2. Avoid unnecessary memory allocations and full-directory loading.
3. Handle errors explicitly and avoid silently ignoring filesystem inconsistencies.
4. Preserve relevant filesystem metadata wherever possible.
5. Prefer safe, read-only operations before implementing write capabilities.
6. Keep interfaces between components modular and maintainable.
7. Test filesystem implementations against real-world images and edge cases.

### Formatting

Format the workspace with:

```bash
cargo fmt --all
```

### Linting

Run Clippy with:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

### Testing

Run the complete test suite with:

```bash
cargo test --workspace
```

---

## Roadmap

The following roadmap describes the intended development direction. It is not a guarantee of release dates or feature availability.

- [ ] Establish the application and library architecture.
- [ ] Build the initial desktop interface.
- [ ] Implement disk and volume discovery.
- [ ] Implement foundational filesystem abstractions.
- [ ] Add HFS+ directory browsing.
- [ ] Add HFS and HFSX support.
- [ ] Implement file metadata inspection.
- [ ] Introduce a built-in file preview system.
- [ ] Add fast search and indexing.
- [ ] Implement the plugin architecture.
- [ ] Investigate and implement APFS support.
- [ ] Improve cross-platform compatibility.
- [ ] Introduce additional file management capabilities.

The roadmap will be updated as milestones are completed.

---

## Contributing

Contributions, suggestions, bug reports, and feature requests are welcome.

Whether you want to improve filesystem compatibility, optimize performance, work on the interface, or help with documentation, contributions are appreciated.

### How to contribute

1. Fork the repository.
2. Create a new branch for your changes.
3. Implement and test your changes.
4. Format your code using `cargo fmt`.
5. Submit a pull request describing your changes.

For larger changes, opening an issue first is recommended so the proposed implementation can be discussed.

Contributors are listed in [CONTRIBUTORS.md](CONTRIBUTORS.md).

---

## License

Copyright (C) 2026 Julian A. Loy.

Disk Atlas is free and open-source software licensed under the **GNU General Public License v3.0 or later (GPL-3.0-or-later)**.

You are free to use, study, modify, and redistribute the software under the terms of the license.

See the [LICENSE](LICENSE) file for the complete license text.

---

## Acknowledgements

Disk Atlas is made possible by the open-source ecosystem and the developers maintaining the tools and libraries it builds upon.

Special thanks to the Rust community and the developers behind GPUI and its supporting projects.

---

## Contact

**Author:** Julian A. Loy

**Repository:** [github.com/05Narwhal/DiskAtlas](https://github.com/05Narwhal/DiskAtlas)

For questions, bug reports, feature requests, and contributions, please use the repository's GitHub Issues and Discussions.

---

<p align="center">
  <strong>Disk Atlas</strong><br>
  Explore beyond filesystem boundaries.
</p>