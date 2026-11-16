// Disk Atlas - A cross-platform filesystem explorer.
//
// Copyright (C) 2026 Julian A. Loy.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
//
// See the LICENSE file for the full license text.

//! What the sidebar lists: the places the app can browse.
//!
//! The host's home folder and drives, physical disks and opened disk images. A disk or an image
//! is a [`Disk`] that groups one [`Source`] per volume. A volume whose format has a reader is
//! mounted right after probing; every other volume is [`Availability::Unsupported`] (or
//! [`Availability::Failed`] when mounting failed) and backed by [`Unavailable`].
//!
//! Everything that touches a device or an image runs on the background executor. The functions
//! that decide what a disk contains ([`lay_out`], [`prepare`]) are plain and GPUI-free so they can
//! be tested with in-memory disks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use disk_atlas_lib::error::{Error, Result as EngineResult};
use disk_atlas_lib::fs::native::{self, NativeFs};
use disk_atlas_lib::fs::probe::{
    ContainerVolume, Content, Mount, VolumeRole, VolumeState, mount as mount_content,
    probe as probe_content,
};
use disk_atlas_lib::fs::{DirStream, FileSystem, Metadata, NodeId, SnapshotInfo, VolumeInfo};
use disk_atlas_lib::image;
use disk_atlas_lib::io::block::{BlockSource, Slice};
use disk_atlas_lib::io::cache::{CachedSource, ChunkCache, DEFAULT_BUDGET};
use disk_atlas_lib::io::device::{self, DeviceInfo, HostVolume};
use disk_atlas_lib::partition::{self, Guid, Partition, PartitionType, Scheme, TypeHint};
use gpui::{Context, EventEmitter, SharedString, Task};

/// Names a source for as long as the app runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceId(u32);

impl SourceId {
    /// A fixed id for tests.
    #[cfg(test)]
    pub fn for_test(n: u32) -> Self {
        Self(n)
    }
}

/// Names a disk or an opened image for as long as the app runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiskId(u32);

impl DiskId {
    /// A fixed id for tests.
    #[cfg(test)]
    pub fn for_test(n: u32) -> Self {
        Self(n)
    }
}

/// The sidebar group a source is listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceGroup {
    /// Physical disks and their volumes.
    Disks,
    /// Opened disk images.
    DiskImages,
    /// Folders and drives of the host.
    ThisComputer,
}

impl SourceGroup {
    /// The group's heading in the sidebar.
    pub fn title(self) -> &'static str {
        match self {
            Self::Disks => "Disks",
            Self::DiskImages => "Disk images",
            Self::ThisComputer => "This computer",
        }
    }
}

/// Where a source comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A folder or drive of the host.
    Host,
    /// One volume of a disk or image: the bytes `start..start + len` of it.
    Volume {
        /// The disk or image it lies on.
        disk: DiskId,
        /// Byte offset of the volume on the disk.
        start: u64,
        /// Length of the volume in bytes.
        len: u64,
    },
    /// A whole disk or image that could not be opened or whose map could not be read.
    WholeDisk {
        /// The disk or image.
        disk: DiskId,
    },
}

/// A partition the computer needs and a person does not browse: the sidebar keeps these behind
/// "other partitions" even when Disk Atlas can read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemKind {
    /// The EFI system partition.
    Efi,
    /// Microsoft's reserved partition.
    MicrosoftReserved,
    /// An Apple boot or recovery partition.
    AppleBoot,
    /// The BIOS boot partition of a GPT disk.
    BiosBoot,
}

/// Apple boot and recovery (`426F6F74-0000-11AA-AA11-00306543ECAC`).
const APPLE_BOOT: Guid = Guid::from_fields(
    0x426F_6F74,
    0x0000,
    0x11AA,
    [0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC],
);
/// Apple TV recovery (`5265636F-7665-11AA-AA11-00306543ECAC`).
const APPLE_TV_RECOVERY: Guid = Guid::from_fields(
    0x5265_636F,
    0x7665,
    0x11AA,
    [0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC],
);
/// Microsoft reserved (`E3C9E316-0B5C-4DB8-817D-F92DF00215AE`).
const MICROSOFT_RESERVED: Guid = Guid::from_fields(
    0xE3C9_E316,
    0x0B5C,
    0x4DB8,
    [0x81, 0x7D, 0xF9, 0x2D, 0xF0, 0x02, 0x15, 0xAE],
);
/// BIOS boot (`21686148-6449-6E6F-744E-656564454649`).
const BIOS_BOOT: Guid = Guid::from_fields(
    0x2168_6148,
    0x6449,
    0x6E6F,
    [0x74, 0x4E, 0x65, 0x65, 0x64, 0x45, 0x46, 0x49],
);

impl SystemKind {
    /// What the partition is called when its map gives it no name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Efi => "EFI System",
            Self::MicrosoftReserved => "Microsoft Reserved",
            Self::AppleBoot => "Apple Boot",
            Self::BiosBoot => "BIOS Boot",
        }
    }

    /// One line, in plain words, of why the sidebar keeps it out of the way.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Efi => {
                "EFI system partition. The computer starts up from it; it holds no files of yours."
            }
            Self::MicrosoftReserved => {
                "Microsoft reserved partition. Windows keeps it empty on purpose."
            }
            Self::AppleBoot => {
                "Apple boot or recovery partition. macOS uses it to start up and repair itself."
            }
            Self::BiosBoot => "BIOS boot partition. A boot loader uses it; it holds no files.",
        }
    }

    /// The system kind a partition's type marks, if any.
    fn of(partition: &Partition) -> Option<Self> {
        match &partition.kind {
            PartitionType::Gpt(guid) if *guid == MICROSOFT_RESERVED => {
                return Some(Self::MicrosoftReserved);
            }
            PartitionType::Gpt(guid) if *guid == BIOS_BOOT => return Some(Self::BiosBoot),
            PartitionType::Gpt(guid) if *guid == APPLE_BOOT || *guid == APPLE_TV_RECOVERY => {
                return Some(Self::AppleBoot);
            }
            PartitionType::Apm(kind) if kind.starts_with("Apple_Boot") => {
                return Some(Self::AppleBoot);
            }
            _ => {}
        }
        (partition.hint == TypeHint::EfiSystem).then_some(Self::Efi)
    }
}

/// Whether the app can browse a source, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// The filesystem can be listed.
    Browsable,
    /// The volume holds a format that has no reader yet.
    Unsupported {
        /// The format's name for the user ("HFS+").
        format: SharedString,
    },
    /// Reading the disk needs administrator rights.
    NeedsElevation,
    /// The disk or image could not be opened or read. The text says why, for the user.
    Failed(SharedString),
}

impl Availability {
    /// True when the source can be listed.
    pub fn is_browsable(&self) -> bool {
        matches!(self, Self::Browsable)
    }
}

/// One browsable place.
#[derive(Clone)]
pub struct Source {
    /// Identifies the source.
    pub id: SourceId,
    /// The sidebar group it belongs to.
    pub group: SourceGroup,
    /// What the sidebar calls it.
    pub label: SharedString,
    /// The filesystem's name shown at the right of the sidebar row ("HFS+"). `None` for host
    /// folders and for a whole disk that could not be read.
    pub format: Option<SharedString>,
    /// The filesystem to read: a real one, or [`Unavailable`] when `availability` says no.
    pub fs: Arc<dyn FileSystem>,
    /// Where the source comes from.
    pub kind: SourceKind,
    /// Whether it can be browsed.
    pub availability: Availability,
    /// Set for a partition the computer needs rather than a person (EFI, boot, reserved).
    pub system: Option<SystemKind>,
    /// What a volume of a container (APFS) is for, when the container says. The sidebar orders
    /// and dims by it; it never decides what can be read.
    pub role: Option<VolumeRole>,
    /// Set for an earlier state of a volume, opened from its snapshot row. Such a source is not
    /// one of its disk's `volumes`: the sidebar lists it under its volume.
    pub snapshot: Option<SnapshotOf>,
}

/// What a snapshot source shows: an earlier state of `volume`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotOf {
    /// The volume it is a snapshot of.
    pub volume: SourceId,
    /// The snapshot's id on that volume.
    pub id: u64,
}

/// `snapshots` newest first; one without a date last. Ties keep the higher id first.
fn newest_first(mut snapshots: Vec<SnapshotInfo>) -> Vec<SnapshotInfo> {
    snapshots.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
    snapshots
}

/// Where a volume of a container goes among its siblings: Data and volumes without a role first,
/// then System, then Preboot, Recovery and VM (design/m7-apfs.md §8).
pub fn role_rank(role: Option<VolumeRole>) -> u8 {
    match role {
        Some(VolumeRole::System) => 1,
        Some(VolumeRole::Preboot) => 2,
        Some(VolumeRole::Recovery) => 3,
        Some(VolumeRole::Vm) => 4,
        // `VolumeRole` is non-exhaustive: a role the app does not know yet is listed like Data.
        None | Some(VolumeRole::Data | _) => 0,
    }
}

/// True for the roles the sidebar dims, because they are rarely what anyone wants: Preboot,
/// Recovery and VM.
pub fn is_quiet_role(role: Option<VolumeRole>) -> bool {
    role_rank(role) >= 2
}

/// The volumes of one disk that lie on the same bytes: the volumes of one container, or a volume
/// alone in its partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Byte offset on the disk.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
    /// The volumes, in sidebar order. Never empty.
    pub volumes: Vec<SourceId>,
}

impl Span {
    /// The volume that stands for the span: `current` when it is one of them, else the first.
    pub fn lead(&self, current: Option<SourceId>) -> Option<SourceId> {
        current
            .filter(|id| self.volumes.contains(id))
            .or_else(|| self.volumes.first().copied())
    }

    /// True when `id` is one of the span's volumes.
    pub fn holds(&self, id: Option<SourceId>) -> bool {
        id.is_some_and(|id| self.volumes.contains(&id))
    }
}

/// Groups `(source, start, len)` volumes, in disk order, into spans: neighbours on the same bytes
/// are one span. The volumes of a container are added next to each other, so they meet here.
fn group_spans(volumes: &[(SourceId, u64, u64)]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::with_capacity(volumes.len());
    for &(id, start, len) in volumes {
        match spans.last_mut() {
            Some(span) if span.start == start && span.len == len => span.volumes.push(id),
            _ => spans.push(Span {
                start,
                len,
                volumes: vec![id],
            }),
        }
    }
    spans
}

/// A physical disk or an opened disk image, and the sources it holds.
pub struct Disk {
    /// Identifies the disk.
    pub id: DiskId,
    /// `Disks` or `DiskImages`.
    pub group: SourceGroup,
    /// What the sidebar calls it: the model, or the file name of an image.
    pub label: SharedString,
    /// The device path or the image file, as opened.
    pub path: PathBuf,
    /// Size in bytes; 0 when it could not be opened.
    pub size: u64,
    /// Removable media or attached over USB.
    pub removable: bool,
    /// The kind of partition map. `None`: no map (the whole disk is one volume) or not read.
    pub scheme: Option<Scheme>,
    /// The volumes, in map order.
    pub volumes: Vec<SourceId>,
    /// The source that stands for the whole disk when it could not be opened or its map could
    /// not be read. Then `volumes` is empty.
    pub whole: Option<SourceId>,
}

impl Disk {
    /// The source choosing this disk opens: its first volume, or the whole-disk source.
    pub fn first_source(&self) -> Option<SourceId> {
        self.volumes.first().copied().or(self.whole)
    }
}

/// What changed in [`Sources`].
pub enum SourcesEvent {
    /// The list of sources changed.
    Changed,
    /// The user asked for this source (opening an image) and it is ready to be shown.
    Selected(SourceId),
}

impl EventEmitter<SourcesEvent> for Sources {}

/// The `FileSystem` of a source that cannot be browsed yet. It exists so that every source has an
/// `fs` and `Location` keeps one shape. Nothing lists it: `Listing` reads the source's
/// availability first.
struct Unavailable {
    label: String,
}

impl Unavailable {
    fn arc(label: &str) -> Arc<dyn FileSystem> {
        Arc::new(Self {
            label: label.to_owned(),
        })
    }
}

fn cannot_read_yet() -> Error {
    Error::unsupported("this volume cannot be read yet")
}

impl FileSystem for Unavailable {
    fn info(&self) -> VolumeInfo {
        VolumeInfo::new(self.label.clone())
    }

    fn root(&self) -> NodeId {
        NodeId(0)
    }

    fn metadata(&self, _node: NodeId) -> EngineResult<Metadata> {
        Err(cannot_read_yet())
    }

    fn lookup(&self, _dir: NodeId, _name: &str) -> EngineResult<Option<NodeId>> {
        Err(cannot_read_yet())
    }

    fn read_dir(&self, _dir: NodeId) -> EngineResult<Box<dyn DirStream>> {
        Err(cannot_read_yet())
    }
}

/// A volume found on a disk, before it has an id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VolumePlan {
    label: String,
    start: u64,
    len: u64,
    format: &'static str,
    content: Content,
    system: Option<SystemKind>,
}

/// A volume ready to be added: its plan, and its filesystem when one mounted.
struct PreparedVolume {
    plan: VolumePlan,
    /// `None` unless `availability` is [`Availability::Browsable`].
    fs: Option<Arc<dyn FileSystem>>,
    availability: Availability,
    /// What the volume is for, when it is one of a container's.
    role: Option<VolumeRole>,
}

/// The words for a volume that failed to mount.
fn mount_failure(error: &Error) -> Availability {
    Availability::Failed(format!("This volume could not be read ({error}).").into())
}

/// Mounts the volume `plan` describes when a reader exists for its format. A mounted volume
/// takes the volume's own name when it has one. A container gives one volume per volume in it,
/// all on the partition's bytes, in sidebar order. Blocking.
fn mount_volume(source: &Arc<dyn BlockSource>, mut plan: VolumePlan) -> Vec<PreparedVolume> {
    let volume = if plan.start == 0 && plan.len == source.len() {
        Ok(Arc::clone(source))
    } else {
        Slice::new(Arc::clone(source), plan.start, plan.len)
            .map(|slice| Arc::new(slice) as Arc<dyn BlockSource>)
    };
    let one = match volume.and_then(|volume| mount_content(volume, plan.content)) {
        Ok(Some(Mount::Volume(fs))) => {
            let name = fs.info().name;
            if !name.trim().is_empty() {
                plan.label = name;
            }
            PreparedVolume {
                plan,
                fs: Some(fs),
                availability: Availability::Browsable,
                role: None,
            }
        }
        Ok(Some(Mount::Container(volumes))) => return container_volumes(&plan, volumes),
        // `Mount` is non-exhaustive, hence the wildcard.
        Ok(None | Some(_)) => PreparedVolume {
            availability: Availability::Unsupported {
                format: SharedString::from(plan.format),
            },
            plan,
            fs: None,
            role: None,
        },
        Err(error) => PreparedVolume {
            availability: mount_failure(&error),
            plan,
            fs: None,
            role: None,
        },
    };
    vec![one]
}

/// The volumes of a container on the partition `plan` describes, Data first (see [`role_rank`]).
/// A volume without a name takes the partition's label. An empty container is one failed entry,
/// so the partition does not vanish from the sidebar.
fn container_volumes(plan: &VolumePlan, volumes: Vec<ContainerVolume>) -> Vec<PreparedVolume> {
    if volumes.is_empty() {
        return vec![PreparedVolume {
            plan: plan.clone(),
            fs: None,
            availability: Availability::Failed("This container holds no volumes.".into()),
            role: None,
        }];
    }
    let mut prepared: Vec<PreparedVolume> = volumes
        .into_iter()
        .map(|volume| {
            let mut plan = plan.clone();
            let name = volume.name.trim();
            if !name.is_empty() {
                plan.label = name.to_owned();
            }
            let (fs, availability) = match volume.state {
                VolumeState::Ready(fs) => (Some(fs), Availability::Browsable),
                // The unlock screen is M7 task 14; until then a locked volume says what it is.
                VolumeState::Locked(_) => (
                    None,
                    Availability::Failed(
                        "This volume is encrypted. Disk Atlas cannot unlock it yet.".into(),
                    ),
                ),
                VolumeState::Failed(error) => (None, mount_failure(&error)),
                // `VolumeState` is non-exhaustive.
                _ => (
                    None,
                    Availability::Failed("This volume cannot be read yet.".into()),
                ),
            };
            PreparedVolume {
                plan,
                fs,
                availability,
                role: volume.role,
            }
        })
        .collect();
    // Stable: volumes of one rank keep the container's order.
    prepared.sort_by_key(|volume| role_rank(volume.role));
    prepared
}

/// What a disk's bytes say it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    scheme: Option<Scheme>,
    volumes: Vec<VolumePlan>,
    /// Set when the partition map could not be read: the disk then has no volumes.
    failure: Option<Availability>,
}

/// What `bytes` hold, or [`Content::Unknown`] when the probe cannot read them.
fn content_of(bytes: &dyn BlockSource) -> Content {
    probe_content(bytes).unwrap_or(Content::Unknown)
}

/// Works out the volumes of an opened disk: reads its partition map and probes each partition,
/// or probes the whole disk when there is no map. `source` should already be cached, so the
/// partitions share one set of chunks. Does blocking I/O.
fn lay_out(source: &Arc<dyn BlockSource>, disk_label: &str) -> Layout {
    match partition::read(source.as_ref()) {
        Ok(Some(map)) => {
            let volumes = map
                .partitions
                .iter()
                .filter(|partition| partition.hint != TypeHint::NotData)
                .map(|partition| {
                    let system = SystemKind::of(partition);
                    let label = partition
                        .name
                        .as_deref()
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map_or_else(
                            || {
                                system.map_or_else(
                                    || format!("Partition {}", partition.number),
                                    |kind| kind.name().to_owned(),
                                )
                            },
                            str::to_owned,
                        );
                    let content = partition
                        .slice(Arc::clone(source))
                        .map_or(Content::Unknown, |slice| content_of(&slice));
                    VolumePlan {
                        label,
                        start: partition.start,
                        len: partition.len,
                        format: content.format_name(),
                        content,
                        system,
                    }
                })
                .collect();
            Layout {
                scheme: Some(map.scheme),
                volumes,
                failure: None,
            }
        }
        Ok(None) => {
            let content = content_of(source.as_ref());
            Layout {
                scheme: None,
                volumes: vec![VolumePlan {
                    label: disk_label.to_owned(),
                    start: 0,
                    len: source.len(),
                    format: content.format_name(),
                    content,
                    system: None,
                }],
                failure: None,
            }
        }
        Err(error) => Layout {
            scheme: None,
            volumes: Vec::new(),
            failure: Some(Availability::Failed(
                format!("This disk's partition map could not be read ({error}).").into(),
            )),
        },
    }
}

/// A host root's path as text, without a trailing separator, for matching drive letters.
fn trim_root(path: &Path) -> String {
    path.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_owned()
}

/// Whether `on_disk` host volumes account for a disk with `data` volumes a person would browse.
/// A disk that could not be read (`unreadable`) is never covered: its volumes are not known.
fn host_covers_disk(unreadable: bool, data: usize, on_disk: usize) -> bool {
    !unreadable && on_disk > 0 && on_disk >= data
}

/// What the background executor learned about a disk or image.
struct Found {
    group: SourceGroup,
    label: String,
    path: PathBuf,
    size: u64,
    removable: bool,
}

/// A disk or image ready to be added on the UI thread.
struct PreparedDisk {
    found: Found,
    scheme: Option<Scheme>,
    volumes: Vec<PreparedVolume>,
    /// Why the whole disk is unavailable, when it is.
    whole: Option<Availability>,
}

/// Turns the outcome of opening a disk or image into what the sidebar lists. A disk that could
/// not be opened becomes one unavailable entry; one that opened is laid out and its volumes
/// mounted.
fn prepare(found: Found, opened: Result<Arc<dyn BlockSource>, Availability>) -> PreparedDisk {
    match opened {
        Err(availability) => PreparedDisk {
            found,
            scheme: None,
            volumes: Vec::new(),
            whole: Some(availability),
        },
        Ok(source) => {
            let layout = lay_out(&source, &found.label);
            PreparedDisk {
                found,
                scheme: layout.scheme,
                volumes: layout
                    .volumes
                    .into_iter()
                    .flat_map(|plan| mount_volume(&source, plan))
                    .collect(),
                whole: layout.failure,
            }
        }
    }
}

/// Puts `source` behind the shared cache.
fn cached(
    source: Arc<dyn BlockSource>,
    cache: &Arc<ChunkCache>,
    what: &str,
) -> Result<Arc<dyn BlockSource>, Availability> {
    CachedSource::new(source, Arc::clone(cache))
        .map(|source| Arc::new(source) as Arc<dyn BlockSource>)
        .map_err(|error| {
            Availability::Failed(format!("This {what} could not be read ({error}).").into())
        })
}

/// Opens one physical disk and works out what it holds. Blocking.
fn scan_device(info: &DeviceInfo, cache: &Arc<ChunkCache>) -> PreparedDisk {
    let label = info
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map_or_else(|| info.path.display().to_string(), str::to_owned);
    let opened = match device::open(&info.path) {
        Ok(device) => cached(Arc::new(device), cache, "disk"),
        Err(Error::AccessDenied { .. }) => Err(Availability::NeedsElevation),
        Err(error) => Err(Availability::Failed(
            format!("This disk could not be opened ({error}).").into(),
        )),
    };
    prepare(
        Found {
            group: SourceGroup::Disks,
            label,
            path: info.path.clone(),
            size: info.size,
            removable: info.removable,
        },
        opened,
    )
}

/// The user-facing reason an image could not be opened.
///
/// An `Unsupported` error names what is missing itself: a whole format ("DiskCopy 4.2 files") or
/// one feature of a format that has a reader ("segmented disk images", "UDIF version 3").
fn image_failure(error: &Error) -> Availability {
    let text = match error {
        Error::Unsupported(what) => format!("Disk Atlas cannot read {what} yet."),
        Error::AccessDenied { .. } => "Disk Atlas is not allowed to open this image.".to_owned(),
        other => format!("This image could not be opened ({other})."),
    };
    Availability::Failed(text.into())
}

/// Opens one disk image and works out what it holds. Blocking.
fn scan_image(path: PathBuf, cache: &Arc<ChunkCache>) -> PreparedDisk {
    let label = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let opened = match image::open(&path) {
        Ok(opened) => cached(opened.source, cache, "image"),
        Err(error) => Err(image_failure(&error)),
    };
    let size = opened.as_ref().map_or(0, |source| source.len());
    prepare(
        Found {
            group: SourceGroup::DiskImages,
            label,
            path,
            size,
            removable: false,
        },
        opened,
    )
}

/// A host root probed on the background executor, not yet given an id.
struct Probed {
    path: PathBuf,
    label: String,
    fs: Arc<dyn FileSystem>,
}

/// Opens one host root, or logs why it was skipped.
fn probe(path: PathBuf, label: String) -> Option<Probed> {
    match NativeFs::new(path.clone(), label.clone()) {
        Ok(fs) => Some(Probed {
            path,
            label,
            fs: Arc::new(fs),
        }),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skipping a host root that cannot be opened");
            None
        }
    }
}

/// The set of sources the app can browse.
pub struct Sources {
    items: Vec<Source>,
    disks: Vec<Disk>,
    /// Host paths already added, so the two startup tasks never add a root twice.
    paths: Vec<PathBuf>,
    /// The source of each host root, by its path.
    host_roots: Vec<(SourceId, PathBuf)>,
    /// Which physical disk each host drive letter lives on. Empty until the lookup is done.
    host_volumes: Vec<HostVolume>,
    next_id: u32,
    next_disk: u32,
    /// The one chunk cache every device and image reads through.
    cache: Arc<ChunkCache>,
    /// The snapshots of each volume that has any, newest first. Asked once, after mounting.
    snapshots: HashMap<SourceId, Arc<[SnapshotInfo]>>,
    /// The source of each snapshot opened so far, by (volume, snapshot id): kept for the session.
    snapshot_sources: HashMap<(SourceId, u64), SourceId>,
    /// Counts snapshot openings; only the latest selects its source when it is ready.
    snapshot_request: u64,
    _tasks: Vec<Task<()>>,
}

impl Sources {
    /// Starts looking for the host's roots and the physical disks in the background.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let cache = Arc::new(ChunkCache::new(DEFAULT_BUDGET));

        // Probing drive letters can be slow, so the home folder gets its own, quicker task.
        let home_task = cx.spawn(async move |this, cx| {
            let home = cx
                .background_executor()
                .spawn(async {
                    std::env::home_dir()
                        .and_then(|path| probe(path, String::from("Home")))
                        .into_iter()
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| this.add_roots(home, cx)).ok();
        });
        let roots_task = cx.spawn(async move |this, cx| {
            let roots = cx
                .background_executor()
                .spawn(async {
                    native::host_roots()
                        .into_iter()
                        .filter_map(|root| probe(root.path, root.label))
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| this.add_roots(roots, cx)).ok();
        });
        // Each disk is added as soon as it has been read, so a slow one does not hold up the rest.
        let disks_task = cx.spawn({
            let cache = Arc::clone(&cache);
            async move |this, cx| {
                let listed = cx
                    .background_executor()
                    .spawn(async { device::list() })
                    .await;
                let devices = match listed {
                    Ok(devices) => devices,
                    Err(error) => {
                        tracing::warn!(%error, "could not list the physical disks");
                        return;
                    }
                };
                for info in devices {
                    let cache = Arc::clone(&cache);
                    let prepared = cx
                        .background_executor()
                        .spawn(async move { scan_device(&info, &cache) })
                        .await;
                    if this
                        .update(cx, |this, cx| this.add_disk(prepared, cx))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
        // Which disk each drive letter is on: a quick query that needs no rights.
        let volumes_task = cx.spawn(async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async { device::host_volumes() })
                .await;
            match listed {
                Ok(volumes) if !volumes.is_empty() => {
                    this.update(cx, |this, cx| {
                        this.host_volumes = volumes;
                        cx.notify();
                    })
                    .ok();
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not map the host drives to their disks");
                }
            }
        });
        Self {
            items: Vec::new(),
            disks: Vec::new(),
            paths: Vec::new(),
            host_roots: Vec::new(),
            host_volumes: Vec::new(),
            next_id: 0,
            next_disk: 0,
            cache,
            snapshots: HashMap::new(),
            snapshot_sources: HashMap::new(),
            snapshot_request: 0,
            _tasks: vec![home_task, roots_task, disks_task, volumes_task],
        }
    }

    /// Every source, in the order they were added.
    pub fn items(&self) -> &[Source] {
        &self.items
    }

    /// Every disk and image, in the order they were added.
    pub fn disks(&self) -> &[Disk] {
        &self.disks
    }

    /// The disk called `id`.
    pub fn disk(&self, id: DiskId) -> Option<&Disk> {
        self.disks.iter().find(|disk| disk.id == id)
    }

    /// The volumes of `disk` by the bytes they lie on, in map order: the volumes of a container
    /// are one span. Empty for a disk that failed as a whole.
    pub fn spans(&self, disk: &Disk) -> Vec<Span> {
        let volumes: Vec<(SourceId, u64, u64)> = disk
            .volumes
            .iter()
            .filter_map(|&id| match self.source(id)?.kind {
                SourceKind::Volume { start, len, .. } => Some((id, start, len)),
                SourceKind::Host | SourceKind::WholeDisk { .. } => None,
            })
            .collect();
        group_spans(&volumes)
    }

    /// Adds the host folder `path` as a source, as the startup tasks do for a drive. For the
    /// tests of the host's live sizes, which need a source whose root they own.
    #[cfg(test)]
    pub fn add_host_for_test(&mut self, path: PathBuf, cx: &mut Context<Self>) -> Option<SourceId> {
        let before = self.host_roots.len();
        let probed = probe(path, String::from("Test"))?;
        self.add_roots(vec![probed], cx);
        (self.host_roots.len() > before)
            .then(|| self.host_roots.last().map(|(id, _)| *id))
            .flatten()
    }

    /// The host folder that is the top of the host source `id`.
    pub fn host_root(&self, id: SourceId) -> Option<&std::path::Path> {
        self.host_roots
            .iter()
            .find(|(source, _)| *source == id)
            .map(|(_, root)| root.as_path())
    }

    /// The label of the physical disk (its model) under the host root `id`, when the host mounts
    /// that root from a disk Disk Atlas listed.
    pub fn host_disk_label(&self, id: SourceId) -> Option<SharedString> {
        let (_, root) = self.host_roots.iter().find(|(source, _)| *source == id)?;
        let root = trim_root(root);
        let volume = self
            .host_volumes
            .iter()
            .find(|volume| trim_root(&volume.root).eq_ignore_ascii_case(&root))?;
        self.disks
            .iter()
            .find(|disk| disk.path == volume.disk)
            .map(|disk| disk.label.clone())
    }

    /// True when the host already mounts every volume of `disk`, so the sidebar lists its
    /// volumes under This computer and not under Disks.
    pub fn is_host_mounted(&self, disk: &Disk) -> bool {
        let on_disk = self
            .host_volumes
            .iter()
            .filter(|volume| volume.disk == disk.path)
            .count();
        let data = disk
            .volumes
            .iter()
            .filter_map(|&id| self.source(id))
            .filter(|source| source.system.is_none())
            .count();
        host_covers_disk(disk.whole.is_some(), data, on_disk)
    }

    /// The source called `id`.
    pub fn source(&self, id: SourceId) -> Option<&Source> {
        self.items.iter().find(|source| source.id == id)
    }

    /// The volume `id` stands for on its disk: the live volume of a snapshot, else `id` itself.
    pub fn live_volume(&self, id: SourceId) -> SourceId {
        self.source(id)
            .and_then(|source| source.snapshot)
            .map_or(id, |snapshot| snapshot.volume)
    }

    /// The snapshots of `volume`, newest first, once listed. `None` when it has none.
    pub fn snapshots(&self, volume: SourceId) -> Option<&Arc<[SnapshotInfo]>> {
        self.snapshots.get(&volume)
    }

    /// The source of snapshot `id` of `volume`, when it has been opened.
    pub fn snapshot_source(&self, volume: SourceId, id: u64) -> Option<SourceId> {
        self.snapshot_sources.get(&(volume, id)).copied()
    }

    /// Opens snapshot `id` of `volume` in the background as a source of its own, then
    /// [`SourcesEvent::Selected`] names it. One already opened is selected at once. When the user
    /// picks another snapshot before this one is ready, this one is kept but not selected.
    pub fn open_snapshot(&mut self, volume: SourceId, id: u64, cx: &mut Context<Self>) {
        self.snapshot_request = self.snapshot_request.wrapping_add(1);
        if let Some(existing) = self.snapshot_source(volume, id) {
            cx.emit(SourcesEvent::Selected(existing));
            return;
        }
        let Some(parent) = self.source(volume).cloned() else {
            return;
        };
        let request = self.snapshot_request;
        let fs = Arc::clone(&parent.fs);
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_executor()
                .spawn(async move { fs.open_snapshot(id) })
                .await;
            this.update(cx, |this, cx| {
                // The same snapshot asked twice while it opened: the first one in wins.
                let added = match this.snapshot_source(volume, id) {
                    Some(existing) => existing,
                    None => this.add_snapshot(&parent, id, opened),
                };
                cx.emit(SourcesEvent::Changed);
                if this.snapshot_request == request {
                    cx.emit(SourcesEvent::Selected(added));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Adds the source of snapshot `id` of `parent`: browsable when it opened, else failed with
    /// the reason, so selecting it says what went wrong.
    fn add_snapshot(
        &mut self,
        parent: &Source,
        id: u64,
        opened: EngineResult<Arc<dyn FileSystem>>,
    ) -> SourceId {
        let source_id = self.next_source_id();
        let name = self
            .snapshots(parent.id)
            .and_then(|list| list.iter().find(|snapshot| snapshot.id == id))
            .map_or_else(
                || format!("snapshot {id}"),
                |snapshot| snapshot.name.clone(),
            );
        let (fs, availability, label) = match opened {
            Ok(fs) => {
                let label = fs.info().name;
                (fs, Availability::Browsable, label)
            }
            Err(error) => {
                let label = format!("{} \u{2014} {name}", parent.label);
                (
                    Unavailable::arc(&label),
                    Availability::Failed(
                        format!("This snapshot could not be read ({error}).").into(),
                    ),
                    label,
                )
            }
        };
        self.items.push(Source {
            id: source_id,
            group: parent.group,
            label: SharedString::from(label),
            format: parent.format.clone(),
            fs,
            kind: parent.kind,
            availability,
            system: parent.system,
            role: parent.role,
            snapshot: Some(SnapshotOf {
                volume: parent.id,
                id,
            }),
        });
        self.snapshot_sources.insert((parent.id, id), source_id);
        source_id
    }

    /// Asks every browsable volume in `volumes` for its snapshots, on the background executor,
    /// and records the ones that have any in one update.
    fn list_snapshots(&mut self, volumes: &[SourceId], cx: &mut Context<Self>) {
        let asked: Vec<(SourceId, Arc<dyn FileSystem>)> = volumes
            .iter()
            .filter_map(|&id| self.source(id))
            .filter(|source| source.availability.is_browsable())
            .map(|source| (source.id, Arc::clone(&source.fs)))
            .collect();
        if asked.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    asked
                        .into_iter()
                        .filter_map(|(id, fs)| match fs.snapshots() {
                            Ok(list) if !list.is_empty() => Some((id, newest_first(list))),
                            Ok(_) => None,
                            Err(error) => {
                                tracing::debug!(%error, "could not list a volume's snapshots");
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            if found.is_empty() {
                return;
            }
            this.update(cx, |this, cx| {
                for (id, list) in found {
                    this.snapshots.insert(id, list.into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens the disk image at `path` in the background and adds it to Disk images; then
    /// [`SourcesEvent::Selected`] names its first source. An image already listed is not added
    /// again: it is selected.
    pub fn open_image(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(first) = self.find_image(&path) {
            cx.emit(SourcesEvent::Selected(first));
            return;
        }
        let cache = Arc::clone(&self.cache);
        cx.spawn(async move |this, cx| {
            let prepared = cx
                .background_executor()
                .spawn(async move { scan_image(path, &cache) })
                .await;
            this.update(cx, |this, cx| {
                if let Some(first) = this.add_disk(prepared, cx) {
                    cx.emit(SourcesEvent::Selected(first));
                }
            })
            .ok();
        })
        .detach();
    }

    /// The first source of the image already listed for `path`.
    fn find_image(&self, path: &Path) -> Option<SourceId> {
        self.disks
            .iter()
            .find(|disk| disk.group == SourceGroup::DiskImages && disk.path == path)
            .and_then(Disk::first_source)
    }

    fn next_source_id(&mut self) -> SourceId {
        let id = SourceId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Adds a disk and its sources and tells observers, one notify for the whole disk. A disk
    /// already listed (the same image opened twice while the first was still being read) is not
    /// added again. Returns the first source of the disk, new or existing.
    fn add_disk(&mut self, prepared: PreparedDisk, cx: &mut Context<Self>) -> Option<SourceId> {
        let PreparedDisk {
            found,
            scheme,
            volumes,
            whole,
        } = prepared;
        if let Some(existing) = self
            .disks
            .iter()
            .find(|disk| disk.group == found.group && disk.path == found.path)
        {
            return existing.first_source();
        }

        let disk = DiskId(self.next_disk);
        self.next_disk = self.next_disk.saturating_add(1);
        let label = SharedString::from(found.label);

        let mut volume_ids = Vec::with_capacity(volumes.len());
        for PreparedVolume {
            plan,
            fs,
            availability,
            role,
        } in volumes
        {
            let id = self.next_source_id();
            volume_ids.push(id);
            self.items.push(Source {
                id,
                group: found.group,
                fs: fs.unwrap_or_else(|| Unavailable::arc(&plan.label)),
                label: SharedString::from(plan.label),
                format: Some(SharedString::from(plan.format)),
                kind: SourceKind::Volume {
                    disk,
                    start: plan.start,
                    len: plan.len,
                },
                availability,
                system: plan.system,
                role,
                snapshot: None,
            });
        }
        let whole = whole.map(|availability| {
            let id = self.next_source_id();
            self.items.push(Source {
                id,
                group: found.group,
                label: label.clone(),
                format: None,
                fs: Unavailable::arc(&label),
                kind: SourceKind::WholeDisk { disk },
                availability,
                system: None,
                role: None,
                snapshot: None,
            });
            id
        });
        self.disks.push(Disk {
            id: disk,
            group: found.group,
            label,
            path: found.path,
            size: found.size,
            removable: found.removable,
            scheme,
            volumes: volume_ids,
            whole,
        });
        let first = self.disks.last().and_then(Disk::first_source);
        let listed = self
            .disks
            .last()
            .map(|disk| disk.volumes.clone())
            .unwrap_or_default();
        self.list_snapshots(&listed, cx);
        cx.emit(SourcesEvent::Changed);
        cx.notify();
        first
    }

    /// Appends the roots not already present, then tells observers.
    fn add_roots(&mut self, roots: Vec<Probed>, cx: &mut Context<Self>) {
        let before = self.items.len();
        for root in roots {
            if self.paths.contains(&root.path) {
                continue;
            }
            let id = self.next_source_id();
            self.host_roots.push((id, root.path.clone()));
            self.paths.push(root.path);
            self.items.push(Source {
                id,
                group: SourceGroup::ThisComputer,
                label: SharedString::from(root.label),
                format: None,
                fs: root.fs,
                kind: SourceKind::Host,
                availability: Availability::Browsable,
                system: None,
                role: None,
                snapshot: None,
            });
        }
        if self.items.len() != before {
            cx.emit(SourcesEvent::Changed);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_atlas_lib::io::block::MemSource;
    use disk_atlas_lib::partition::Guid;

    const SS: usize = 512;
    const HFS_PLUS_AT: usize = 1024;

    /// CRC-32 (IEEE), the checksum GPT headers and entry arrays carry.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    /// One GPT entry: type, first and last sector (inclusive), name.
    struct Entry {
        type_guid: Guid,
        first: u64,
        last: u64,
        name: &'static str,
    }

    /// A disk of `total_sectors` 512-byte sectors with a primary and a backup GPT.
    fn gpt_disk(total_sectors: usize, entries: &[Entry]) -> Vec<u8> {
        const COUNT: usize = 128;
        let mut disk = vec![0u8; SS * total_sectors];
        let mut array = vec![0u8; COUNT * 128];
        for (i, entry) in entries.iter().enumerate() {
            let e = &mut array[i * 128..(i + 1) * 128];
            e[..16].copy_from_slice(&entry.type_guid.0);
            e[16..32].copy_from_slice(&[i as u8 + 1; 16]);
            e[32..40].copy_from_slice(&entry.first.to_le_bytes());
            e[40..48].copy_from_slice(&entry.last.to_le_bytes());
            for (j, unit) in entry.name.encode_utf16().enumerate() {
                e[56 + j * 2..58 + j * 2].copy_from_slice(&unit.to_le_bytes());
            }
        }
        let array_sectors = array.len().div_ceil(SS);
        let last_lba = total_sectors - 1;
        let backup_array_lba = last_lba - array_sectors;
        let array_crc = crc32(&array);
        let header = |my: usize, alt: usize, entries_lba: usize| {
            let mut h = vec![0u8; 92];
            h[..8].copy_from_slice(b"EFI PART");
            h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
            h[12..16].copy_from_slice(&92u32.to_le_bytes());
            h[24..32].copy_from_slice(&(my as u64).to_le_bytes());
            h[32..40].copy_from_slice(&(alt as u64).to_le_bytes());
            h[40..48].copy_from_slice(&((2 + array_sectors) as u64).to_le_bytes());
            h[48..56].copy_from_slice(&((backup_array_lba - 1) as u64).to_le_bytes());
            h[56..72].copy_from_slice(&[0xD1; 16]);
            h[72..80].copy_from_slice(&(entries_lba as u64).to_le_bytes());
            h[80..84].copy_from_slice(&(COUNT as u32).to_le_bytes());
            h[84..88].copy_from_slice(&128u32.to_le_bytes());
            h[88..92].copy_from_slice(&array_crc.to_le_bytes());
            let crc = crc32(&h);
            h[16..20].copy_from_slice(&crc.to_le_bytes());
            h
        };
        let primary = header(1, last_lba, 2);
        disk[SS..SS + 92].copy_from_slice(&primary);
        disk[2 * SS..2 * SS + array.len()].copy_from_slice(&array);
        let backup = header(last_lba, 1, backup_array_lba);
        disk[last_lba * SS..last_lba * SS + 92].copy_from_slice(&backup);
        disk[backup_array_lba * SS..backup_array_lba * SS + array.len()].copy_from_slice(&array);
        disk
    }

    /// Writes an HFS+ volume header signature at the start of the volume at `volume_start`.
    fn put_hfs_plus(disk: &mut [u8], volume_start: usize) {
        let at = volume_start + HFS_PLUS_AT;
        disk[at..at + 2].copy_from_slice(b"H+");
        disk[at + 2..at + 4].copy_from_slice(&4u16.to_be_bytes());
    }

    fn mem(bytes: Vec<u8>) -> Arc<dyn BlockSource> {
        Arc::new(MemSource::new(bytes))
    }

    fn entry(type_guid: Guid, first: u64, last: u64, name: &'static str) -> Entry {
        Entry {
            type_guid,
            first,
            last,
            name,
        }
    }

    #[test]
    fn gpt_volumes_get_names_and_formats() {
        let mut disk = gpt_disk(
            4096,
            &[
                entry(Guid::HFS_PLUS, 64, 999, "Macintosh HD"),
                entry(Guid::EFI_SYSTEM, 1000, 1999, ""),
            ],
        );
        put_hfs_plus(&mut disk, 64 * SS);
        let layout = lay_out(&mem(disk), "Samsung T7");
        assert_eq!(layout.scheme, Some(Scheme::Gpt));
        assert_eq!(layout.failure, None);
        assert_eq!(
            layout.volumes,
            vec![
                VolumePlan {
                    label: "Macintosh HD".into(),
                    start: 64 * 512,
                    len: 936 * 512,
                    format: "HFS+",
                    content: Content::HfsPlus,
                    system: None,
                },
                // No name in the map: a system partition takes its kind's name. All zeros: Empty.
                VolumePlan {
                    label: "EFI System".into(),
                    start: 1000 * 512,
                    len: 1000 * 512,
                    format: "Empty",
                    content: Content::Empty,
                    system: Some(SystemKind::Efi),
                },
            ]
        );
    }

    /// A GPT type GUID from its text form: the first three fields are stored little-endian.
    fn guid(text: &str) -> Guid {
        let hex: Vec<u8> = text
            .replace('-', "")
            .as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&[hex[3], hex[2], hex[1], hex[0]]);
        bytes[4..6].copy_from_slice(&[hex[5], hex[4]]);
        bytes[6..8].copy_from_slice(&[hex[7], hex[6]]);
        bytes[8..].copy_from_slice(&hex[8..]);
        Guid(bytes)
    }

    #[test]
    fn system_partitions_are_recognised_by_type() {
        let disk = gpt_disk(
            8192,
            &[
                entry(guid("E3C9E316-0B5C-4DB8-817D-F92DF00215AE"), 64, 99, ""),
                entry(
                    guid("21686148-6449-6E6F-744E-656564454649"),
                    100,
                    199,
                    "boot",
                ),
                entry(
                    guid("426F6F74-0000-11AA-AA11-00306543ECAC"),
                    200,
                    299,
                    "Recovery HD",
                ),
                entry(Guid::LINUX_DATA, 300, 999, "Data"),
            ],
        );
        let layout = lay_out(&mem(disk), "disk");
        let kinds: Vec<_> = layout.volumes.iter().map(|v| v.system).collect();
        assert_eq!(
            kinds,
            [
                Some(SystemKind::MicrosoftReserved),
                Some(SystemKind::BiosBoot),
                Some(SystemKind::AppleBoot),
                None
            ]
        );
        assert_eq!(layout.volumes[0].label, "Microsoft Reserved");
        assert_eq!(layout.volumes[1].label, "boot");
    }

    #[test]
    fn host_coverage_needs_every_data_volume() {
        assert!(host_covers_disk(false, 1, 1));
        assert!(host_covers_disk(false, 2, 3));
        assert!(!host_covers_disk(false, 2, 1));
        assert!(!host_covers_disk(false, 1, 0));
        // A disk that could not be read is never hidden.
        assert!(!host_covers_disk(true, 0, 2));
    }

    #[test]
    fn host_roots_match_without_their_trailing_slash() {
        assert_eq!(trim_root(Path::new("C:\\")), "C:");
        assert_eq!(trim_root(Path::new("C:")), "C:");
    }

    #[test]
    fn a_partition_with_unrecognised_bytes_is_unknown() {
        let mut disk = gpt_disk(
            4096,
            &[
                entry(Guid::LINUX_DATA, 64, 999, "Data"),
                entry(Guid::LINUX_DATA, 1000, 1999, "More"),
            ],
        );
        disk[64 * SS + 5] = 0x42;
        let layout = lay_out(&mem(disk), "disk");
        let formats: Vec<_> = layout.volumes.iter().map(|v| v.format).collect();
        assert_eq!(formats, ["Unknown", "Empty"]);
    }

    /// An Apple partition map with the map itself, a data partition and free space.
    fn apm_disk() -> Vec<u8> {
        let mut disk = vec![0u8; 1 << 20];
        disk[..2].copy_from_slice(b"ER");
        disk[2..4].copy_from_slice(&512u16.to_be_bytes());
        let entries: [(&str, &str, u32, u32); 3] = [
            ("Apple", "Apple_partition_map", 1, 63),
            ("Macintosh HD", "Apple_HFS", 64, 100),
            ("", "Apple_Free", 164, 200),
        ];
        for (i, (name, kind, start, len)) in entries.iter().enumerate() {
            let at = SS * (i + 1);
            let e = &mut disk[at..at + 80];
            e[..2].copy_from_slice(b"PM");
            e[4..8].copy_from_slice(&3u32.to_be_bytes());
            e[8..12].copy_from_slice(&start.to_be_bytes());
            e[12..16].copy_from_slice(&len.to_be_bytes());
            e[16..16 + name.len()].copy_from_slice(name.as_bytes());
            e[48..48 + kind.len()].copy_from_slice(kind.as_bytes());
        }
        put_hfs_plus(&mut disk, 64 * SS);
        disk
    }

    #[test]
    fn apm_not_data_partitions_are_skipped() {
        let layout = lay_out(&mem(apm_disk()), "Old disk");
        assert_eq!(layout.scheme, Some(Scheme::Apm));
        assert_eq!(layout.failure, None);
        assert_eq!(
            layout.volumes,
            vec![VolumePlan {
                label: "Macintosh HD".into(),
                start: 64 * 512,
                len: 100 * 512,
                format: "HFS+",
                content: Content::HfsPlus,
                system: None,
            }]
        );
    }

    #[test]
    fn a_bare_volume_is_one_volume_named_for_the_disk() {
        let mut bytes = vec![0u8; 1 << 20];
        put_hfs_plus(&mut bytes, 0);
        let layout = lay_out(&mem(bytes), "floppy.img");
        assert_eq!(layout.scheme, None);
        assert_eq!(layout.failure, None);
        assert_eq!(
            layout.volumes,
            vec![VolumePlan {
                label: "floppy.img".into(),
                start: 0,
                len: 1 << 20,
                format: "HFS+",
                content: Content::HfsPlus,
                system: None,
            }]
        );
    }

    #[test]
    fn a_damaged_partition_map_fails_the_disk() {
        let mut disk = gpt_disk(2048, &[entry(Guid::HFS_PLUS, 64, 999, "Macintosh HD")]);
        // Spoil both headers' CRCs.
        disk[SS + 48] ^= 0xFF;
        disk[2047 * SS + 48] ^= 0xFF;
        let layout = lay_out(&mem(disk), "Samsung T7");
        assert!(layout.volumes.is_empty());
        assert_eq!(layout.scheme, None);
        let Some(Availability::Failed(text)) = layout.failure else {
            panic!("expected a failure, got {:?}", layout.failure);
        };
        assert!(
            text.starts_with("This disk's partition map could not be read ("),
            "{text}"
        );
        assert!(text.ends_with(")."), "{text}");
    }

    fn found(group: SourceGroup) -> Found {
        Found {
            group,
            label: "Test".into(),
            path: PathBuf::from("test"),
            size: 1 << 20,
            removable: false,
        }
    }

    #[test]
    fn prepare_keeps_an_unopenable_disk_as_one_entry() {
        let prepared = prepare(found(SourceGroup::Disks), Err(Availability::NeedsElevation));
        assert!(prepared.volumes.is_empty());
        assert_eq!(prepared.whole, Some(Availability::NeedsElevation));
        assert_eq!(prepared.scheme, None);
    }

    #[test]
    fn prepare_lays_out_an_opened_disk() {
        let mut disk = gpt_disk(4096, &[entry(Guid::HFS_PLUS, 64, 999, "Mac")]);
        put_hfs_plus(&mut disk, 64 * SS);
        let prepared = prepare(found(SourceGroup::DiskImages), Ok(mem(disk)));
        assert_eq!(prepared.whole, None);
        assert_eq!(prepared.scheme, Some(Scheme::Gpt));
        assert_eq!(prepared.volumes.len(), 1);
    }

    #[test]
    fn unavailable_reads_nothing() {
        let fs = Unavailable::arc("Mac");
        assert_eq!(fs.info().name, "Mac");
        assert_eq!(fs.root(), NodeId(0));
        assert!(matches!(fs.metadata(NodeId(0)), Err(Error::Unsupported(_))));
        assert!(matches!(
            fs.lookup(NodeId(0), "x"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(fs.read_dir(NodeId(0)), Err(Error::Unsupported(_))));
    }

    /// A temp file removed on drop.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir()
                .join(format!("disk-atlas-sources-{}-{name}", std::process::id()));
            std::fs::write(&path, bytes).expect("temp file is written");
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn an_unsupported_feature_of_a_readable_format_is_named() {
        // A UDIF trailer whose version field is 0: the format has a reader, this version does not.
        let mut bytes = vec![0u8; 2048];
        bytes[2048 - 512..2048 - 508].copy_from_slice(b"koly");
        let file = TempFile::new("udif", &bytes);
        let prepared = scan_image(file.0.clone(), &Arc::new(ChunkCache::new(1 << 20)));
        assert_eq!(
            prepared.whole,
            Some(Availability::Failed(
                "Disk Atlas cannot read UDIF version 0 yet.".into()
            ))
        );
    }

    #[test]
    fn an_encrypted_image_is_named() {
        let mut bytes = vec![0u8; 1024];
        bytes[..8].copy_from_slice(b"encrcdsa");
        let file = TempFile::new("encrypted", &bytes);
        let prepared = scan_image(file.0.clone(), &Arc::new(ChunkCache::new(1 << 20)));
        assert_eq!(
            prepared.whole,
            Some(Availability::Failed(
                "Disk Atlas cannot read encrypted disk images yet.".into()
            ))
        );
    }

    #[test]
    fn a_missing_image_fails_with_a_sentence() {
        let path = std::env::temp_dir().join("disk-atlas-sources-does-not-exist.img");
        let prepared = scan_image(path, &Arc::new(ChunkCache::new(1 << 20)));
        let Some(Availability::Failed(text)) = prepared.whole else {
            panic!("expected a failure");
        };
        assert!(
            text.starts_with("This image could not be opened ("),
            "{text}"
        );
    }

    #[test]
    fn a_raw_image_is_laid_out() {
        let mut bytes = vec![0u8; 1 << 20];
        put_hfs_plus(&mut bytes, 0);
        let file = TempFile::new("raw.img", &bytes);
        let prepared = scan_image(file.0.clone(), &Arc::new(ChunkCache::new(1 << 20)));
        assert_eq!(prepared.found.size, 1 << 20);
        assert_eq!(prepared.whole, None);
        assert_eq!(prepared.volumes.len(), 1);
        let volume = &prepared.volumes[0];
        assert_eq!(volume.plan.format, "HFS+");
        assert!(volume.plan.label.ends_with("raw.img"));
        // A signature alone is not a volume: mounting fails, and says so.
        assert!(volume.fs.is_none());
        let Availability::Failed(text) = &volume.availability else {
            panic!("expected a failure, got {:?}", volume.availability);
        };
        assert!(
            text.starts_with("This volume could not be read ("),
            "{text}"
        );
    }

    fn apfs_plan() -> VolumePlan {
        VolumePlan {
            label: "Container disk2".into(),
            start: 4096,
            len: 1 << 30,
            format: "APFS",
            content: Content::Apfs,
            system: None,
        }
    }

    #[test]
    fn a_container_gives_one_volume_each_data_first() {
        let ready = |name: &str, role| {
            ContainerVolume::new(name, role, VolumeState::Ready(Unavailable::arc(name)))
        };
        let volumes = vec![
            ready("Macintosh HD", Some(VolumeRole::System)),
            ready("Preboot", Some(VolumeRole::Preboot)),
            ready("Macintosh HD - Data", Some(VolumeRole::Data)),
            ContainerVolume::new(
                "VM",
                Some(VolumeRole::Vm),
                VolumeState::Failed(Error::unsupported("encrypted volume")),
            ),
            ready("  ", None),
        ];
        let prepared = container_volumes(&apfs_plan(), volumes);
        let rows: Vec<_> = prepared
            .iter()
            .map(|v| (v.plan.label.as_str(), v.role, v.availability.is_browsable()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Macintosh HD - Data", Some(VolumeRole::Data), true),
                // No name: the partition's label.
                ("Container disk2", None, true),
                ("Macintosh HD", Some(VolumeRole::System), true),
                ("Preboot", Some(VolumeRole::Preboot), true),
                ("VM", Some(VolumeRole::Vm), false),
            ]
        );
        // Every volume lies on the partition's bytes and keeps its format.
        assert!(
            prepared
                .iter()
                .all(|v| v.plan.start == 4096 && v.plan.len == 1 << 30 && v.plan.format == "APFS")
        );
        assert!(prepared[..4].iter().all(|v| v.fs.is_some()));
        let Availability::Failed(text) = &prepared[4].availability else {
            panic!("expected a failure");
        };
        assert!(
            text.starts_with("This volume could not be read ("),
            "{text}"
        );
    }

    #[test]
    fn an_empty_container_stays_listed_as_a_failure() {
        let prepared = container_volumes(&apfs_plan(), Vec::new());
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].plan, apfs_plan());
        assert_eq!(
            prepared[0].availability,
            Availability::Failed("This container holds no volumes.".into())
        );
    }

    #[test]
    fn quiet_roles_are_preboot_recovery_and_vm() {
        assert!(!is_quiet_role(None));
        assert!(!is_quiet_role(Some(VolumeRole::Data)));
        assert!(!is_quiet_role(Some(VolumeRole::System)));
        assert!(is_quiet_role(Some(VolumeRole::Preboot)));
        assert!(is_quiet_role(Some(VolumeRole::Recovery)));
        assert!(is_quiet_role(Some(VolumeRole::Vm)));
    }

    #[test]
    fn volumes_on_the_same_bytes_are_one_span() {
        let id = SourceId::for_test;
        let spans = group_spans(&[
            (id(0), 0, 100),
            (id(1), 100, 900),
            (id(2), 100, 900),
            (id(3), 100, 900),
            (id(4), 1000, 50),
        ]);
        assert_eq!(
            spans,
            vec![
                Span {
                    start: 0,
                    len: 100,
                    volumes: vec![id(0)]
                },
                Span {
                    start: 100,
                    len: 900,
                    volumes: vec![id(1), id(2), id(3)]
                },
                Span {
                    start: 1000,
                    len: 50,
                    volumes: vec![id(4)]
                },
            ]
        );
        let shared = &spans[1];
        // The current volume leads its span; otherwise the first in sidebar order.
        assert_eq!(shared.lead(Some(id(3))), Some(id(3)));
        assert_eq!(shared.lead(Some(id(0))), Some(id(1)));
        assert_eq!(shared.lead(None), Some(id(1)));
        assert!(shared.holds(Some(id(2))));
        assert!(!shared.holds(Some(id(4))) && !shared.holds(None));
        assert!(group_spans(&[]).is_empty());
    }

    /// A volume with three snapshots, of which only 7 opens.
    struct Snapshotted;

    impl FileSystem for Snapshotted {
        fn info(&self) -> VolumeInfo {
            VolumeInfo::new("Data")
        }

        fn root(&self) -> NodeId {
            NodeId(0)
        }

        fn metadata(&self, _node: NodeId) -> EngineResult<Metadata> {
            Err(cannot_read_yet())
        }

        fn lookup(&self, _dir: NodeId, _name: &str) -> EngineResult<Option<NodeId>> {
            Err(cannot_read_yet())
        }

        fn read_dir(&self, _dir: NodeId) -> EngineResult<Box<dyn DirStream>> {
            Err(cannot_read_yet())
        }

        fn snapshots(&self) -> EngineResult<Vec<SnapshotInfo>> {
            let snap = |id, name: &str, at: Option<i64>| SnapshotInfo {
                id,
                name: name.to_owned(),
                created: at.map(|seconds| disk_atlas_lib::fs::Timestamp::from_unix(seconds, 0)),
            };
            Ok(vec![
                snap(3, "old", Some(100)),
                snap(5, "undated", None),
                snap(7, "new", Some(200)),
            ])
        }

        fn open_snapshot(&self, id: u64) -> EngineResult<Arc<dyn FileSystem>> {
            if id == 7 {
                Ok(Unavailable::arc("Data \u{2014} new"))
            } else {
                Err(Error::unsupported("a snapshot that holds no data"))
            }
        }
    }

    #[gpui::test]
    fn snapshots_list_newest_first_and_open_as_sources(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.executor().allow_parking();
        let sources = cx.new(Sources::new);
        let selected = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let selected = Rc::clone(&selected);
            cx.subscribe(&sources, move |_, event: &SourcesEvent, _| {
                if let SourcesEvent::Selected(id) = event {
                    selected.borrow_mut().push(*id);
                }
            })
            .detach();
        });
        let volume = sources
            .update(cx, |sources, cx| {
                sources.add_disk(
                    PreparedDisk {
                        found: found(SourceGroup::DiskImages),
                        scheme: None,
                        volumes: vec![PreparedVolume {
                            plan: apfs_plan(),
                            fs: Some(Arc::new(Snapshotted)),
                            availability: Availability::Browsable,
                            role: Some(VolumeRole::Data),
                        }],
                        whole: None,
                    },
                    cx,
                )
            })
            .expect("the disk has a volume");
        selected.borrow_mut().clear();
        cx.run_until_parked();
        let listed = cx.update(|cx| {
            sources
                .read(cx)
                .snapshots(volume)
                .map(|list| list.iter().map(|s| s.id).collect::<Vec<_>>())
        });
        assert_eq!(listed, Some(vec![7, 3, 5]), "newest first, undated last");

        // Two picks in a row: both are kept, only the second is shown.
        sources.update(cx, |sources, cx| {
            sources.open_snapshot(volume, 3, cx);
            sources.open_snapshot(volume, 7, cx);
        });
        cx.run_until_parked();
        let (failed, opened) = cx.update(|cx| {
            let sources = sources.read(cx);
            (
                sources.snapshot_source(volume, 3).expect("3 is kept"),
                sources.snapshot_source(volume, 7).expect("7 is kept"),
            )
        });
        assert_eq!(*selected.borrow(), vec![opened]);
        cx.update(|cx| {
            let sources = sources.read(cx);
            let new = sources.source(opened).unwrap();
            assert_eq!(new.label.as_ref(), "Data \u{2014} new");
            assert!(new.availability.is_browsable());
            assert_eq!(new.snapshot, Some(SnapshotOf { volume, id: 7 }));
            assert_eq!(new.kind, sources.source(volume).unwrap().kind);
            assert_eq!(sources.live_volume(opened), volume);
            assert_eq!(sources.live_volume(volume), volume);
            let old = sources.source(failed).unwrap();
            assert_eq!(old.label.as_ref(), "Container disk2 \u{2014} old");
            let Availability::Failed(text) = &old.availability else {
                panic!("expected a failure");
            };
            assert!(
                text.starts_with("This snapshot could not be read ("),
                "{text}"
            );
            // Snapshots are not volumes of the disk: no row and no span of their own.
            let disk = sources
                .disks()
                .iter()
                .find(|disk| disk.volumes.contains(&volume))
                .unwrap();
            assert_eq!(disk.volumes, vec![volume]);
        });

        // Asked again: the same source, selected at once.
        let count = cx.update(|cx| sources.read(cx).items().len());
        sources.update(cx, |sources, cx| sources.open_snapshot(volume, 3, cx));
        assert_eq!(selected.borrow().last(), Some(&failed));
        assert_eq!(cx.update(|cx| sources.read(cx).items().len()), count);
    }

    #[test]
    fn a_format_without_a_reader_is_unsupported() {
        let mut disk = gpt_disk(4096, &[entry(Guid::LINUX_DATA, 64, 999, "Data")]);
        disk[64 * SS + 5] = 0x42;
        let prepared = prepare(found(SourceGroup::Disks), Ok(mem(disk)));
        assert_eq!(prepared.volumes.len(), 1);
        assert!(prepared.volumes[0].fs.is_none());
        assert_eq!(
            prepared.volumes[0].availability,
            Availability::Unsupported {
                format: "Unknown".into()
            }
        );
    }
}
