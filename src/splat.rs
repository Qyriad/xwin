use crate::{Arch, Ctx, Error, Path, PathBuf, PayloadKind, Variant};
use anyhow::Context as _;
use parking_lot::Mutex;
use rayon::prelude::*;
use std::{collections::HashMap, sync::Arc};

pub struct SplatConfig {
    pub include_debug_libs: bool,
    pub include_debug_symbols: bool,
    pub disable_symlinks: bool,
    pub preserve_ms_arch_notation: bool,
    pub output: PathBuf,
    pub copy: bool,
    //pub isolated: bool,
}

pub(crate) struct SplatRoots {
    crt: PathBuf,
    sdk: PathBuf,
    src: PathBuf,
}

pub(crate) fn prep_splat(
    ctx: std::sync::Arc<Ctx>,
    config: &SplatConfig,
) -> Result<SplatRoots, Error> {
    let crt_root = config.output.join("crt");
    let sdk_root = config.output.join("sdk");

    if crt_root.exists() {
        std::fs::remove_dir_all(&crt_root)
            .with_context(|| format!("unable to delete existing CRT directory {}", crt_root))?;
    }

    if sdk_root.exists() {
        std::fs::remove_dir_all(&sdk_root)
            .with_context(|| format!("unable to delete existing SDK directory {}", sdk_root))?;
    }

    std::fs::create_dir_all(&crt_root)
        .with_context(|| format!("unable to create CRT directory {}", crt_root))?;
    std::fs::create_dir_all(&sdk_root)
        .with_context(|| format!("unable to create SDK directory {}", sdk_root))?;

    let src_root = ctx.work_dir.join("unpack");

    Ok(SplatRoots {
        crt: crt_root,
        sdk: sdk_root,
        src: src_root,
    })
}

pub(crate) fn splat(
    config: &SplatConfig,
    roots: &SplatRoots,
    item: &crate::WorkItem,
    tree: crate::unpack::FileTree,
    arches: u32,
    variants: u32,
    sdk_files: Arc<Mutex<HashMap<u64, PathBuf>>>,
) -> Result<(), Error> {
    struct Mapping<'ft> {
        src: PathBuf,
        target: PathBuf,
        tree: &'ft crate::unpack::FileTree,
        kind: PayloadKind,
        variant: Option<Variant>,
    }

    let mut src = roots.src.join(&item.payload.filename);

    // If we're moving files from the unpack directory, invalidate it immediately
    // so it is recreated in a future run if anyhing goes wrong
    if !config.copy {
        src.push(".unpack");
        if let Err(e) = std::fs::remove_file(&src) {
            tracing::warn!("Failed to remove {}: {}", src, e);
        }
        src.pop();
    }

    let variant = item.payload.variant;
    let kind = item.payload.kind;

    let get_tree = |src_path: &Path| -> Result<&crate::unpack::FileTree, Error> {
        let src_path = src_path
            .strip_prefix(&roots.src)
            .context("incorrect src root")?;
        let src_path = src_path
            .strip_prefix(&item.payload.filename)
            .context("incorrect src subdir")?;

        tree.subtree(src_path)
            .with_context(|| format!("missing expected subtree '{}'", src_path))
    };

    let mappings = match item.payload.kind {
        PayloadKind::CrtHeaders => {
            src.push("include");
            let tree = get_tree(&src)?;

            vec![Mapping {
                src,
                target: roots.crt.join("include"),
                tree,
                kind,
                variant,
            }]
        }
        PayloadKind::CrtLibs => {
            src.push("lib");
            let mut target = roots.crt.join("lib");

            let spectre = (variants & Variant::Spectre as u32) != 0;

            match item
                .payload
                .variant
                .context("CRT libs didn't specify a variant")?
            {
                Variant::Desktop => {
                    if spectre {
                        src.push("spectre");
                        target.push("spectre");
                    }
                }
                Variant::OneCore => {
                    if spectre {
                        src.push("spectre");
                        target.push("spectre");
                    }

                    src.push("onecore");
                    target.push("onecore");
                }
                Variant::Store => {}
                Variant::Spectre => unreachable!(),
            }

            let arch = item
                .payload
                .target_arch
                .context("CRT libs didn't specify an architecture")?;
            src.push(arch.as_ms_str());
            target.push(if config.preserve_ms_arch_notation {
                arch.as_ms_str()
            } else {
                arch.as_str()
            });

            let tree = get_tree(&src)?;

            vec![Mapping {
                src,
                target,
                tree,
                kind,
                variant,
            }]
        }
        PayloadKind::SdkHeaders => {
            src.push("include");
            let tree = get_tree(&src)?;

            vec![Mapping {
                src,
                target: roots.sdk.join("include"),
                tree,
                kind,
                variant,
            }]
        }
        PayloadKind::SdkLibs => {
            src.push("lib/um");
            let mut target = roots.sdk.join("lib/um");

            let arch = item
                .payload
                .target_arch
                .context("SDK libs didn't specify an architecture")?;
            src.push(arch.as_ms_str());
            target.push(if config.preserve_ms_arch_notation {
                arch.as_ms_str()
            } else {
                arch.as_str()
            });

            let tree = get_tree(&src)?;

            vec![Mapping {
                src,
                target,
                tree,
                kind,
                variant,
            }]
        }
        PayloadKind::SdkStoreLibs => {
            src.push("lib/um");
            let target = roots.sdk.join("lib/um");

            Arch::iter(arches)
                .map(|arch| -> Result<Mapping<'_>, Error> {
                    let src = src.join(arch.as_ms_str());
                    let tree = get_tree(&src)?;

                    Ok(Mapping {
                        src,
                        target: target.join(if config.preserve_ms_arch_notation {
                            arch.as_ms_str()
                        } else {
                            arch.as_str()
                        }),
                        tree,
                        kind,
                        variant,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        PayloadKind::Ucrt => {
            let inc_src = src.join("include/ucrt");
            let tree = get_tree(&inc_src)?;

            let mut mappings = vec![Mapping {
                src: inc_src,
                target: roots.sdk.join("include/ucrt"),
                tree,
                kind,
                variant,
            }];

            src.push("lib/ucrt");
            let target = roots.sdk.join("lib/ucrt");
            for arch in Arch::iter(arches) {
                let src = src.join(arch.as_ms_str());
                let tree = get_tree(&src)?;

                mappings.push(Mapping {
                    src,
                    target: target.join(if config.preserve_ms_arch_notation {
                        arch.as_ms_str()
                    } else {
                        arch.as_str()
                    }),
                    tree,
                    kind,
                    variant,
                });
            }

            mappings
        }
    };

    let include_debug_libs = config.include_debug_libs;
    let include_debug_symbols = config.include_debug_symbols;

    let mut results = Vec::new();

    item.progress.reset();
    item.progress
        .set_length(mappings.iter().map(|map| map.tree.stats().1).sum());
    item.progress.set_message("📦 splatting");

    mappings
        .into_par_iter()
        .map(|mapping| -> Result<(), Error> {
            struct Dir<'ft> {
                src: PathBuf,
                tar: PathBuf,
                tree: &'ft crate::unpack::FileTree,
            }

            let mut dir_stack = vec![Dir {
                src: mapping.src,
                tar: mapping.target,
                tree: mapping.tree,
            }];

            while let Some(Dir { src, mut tar, tree }) = dir_stack.pop() {
                std::fs::create_dir_all(&tar)
                    .with_context(|| format!("unable to create {}", tar))?;

                for (fname, size) in &tree.files {
                    // Even if we don't splat 100% of the source files, we still
                    // want to show that we processed them all
                    item.progress.inc(*size);

                    let fnamestr = fname.as_str();
                    if mapping.kind == PayloadKind::CrtLibs || mapping.kind == PayloadKind::Ucrt {
                        if !include_debug_symbols && fname.ends_with(".pdb") {
                            tracing::debug!("skipping {}", fname);
                            continue;
                        }

                        if !include_debug_libs {
                            if let Some(stripped) = fnamestr.strip_suffix(".lib") {
                                if stripped.ends_with("d")
                                    || stripped.ends_with("d_netcore")
                                    || stripped
                                        .strip_suffix(|c: char| c.is_digit(10))
                                        .map_or(false, |fname| fname.ends_with("d"))
                                {
                                    tracing::debug!("skipping {}", fname);
                                    continue;
                                }
                            }
                        }
                    }

                    tar.push(fname);

                    // There is a massive amount of duplication between the
                    // Desktop and Store headers
                    let write = if mapping.kind == PayloadKind::SdkHeaders {
                        let name_hash = calc_lower_hash(fnamestr);

                        let mut lock = sdk_files.lock();
                        if !lock.contains_key(&name_hash) {
                            lock.insert(name_hash, tar.clone());
                            true
                        } else {
                            false
                        }
                    } else {
                        true
                    };

                    if write {
                        let src_path = src.join(fname);

                        if config.copy {
                            std::fs::copy(&src_path, &tar).with_context(|| {
                                format!("failed to copy {} to {}", src_path, tar)
                            })?;
                        } else {
                            std::fs::rename(&src_path, &tar).with_context(|| {
                                format!("failed to move {} to {}", src_path, tar)
                            })?;
                        }

                        match mapping.kind {
                            // These are all internally consistent and lowercased, so if
                            // a library is including them with different casing that is
                            // kind of on them
                            PayloadKind::CrtHeaders | PayloadKind::Ucrt => {}
                            PayloadKind::CrtLibs => {
                                // While _most_ of the libs *stares at Microsoft.VisualC.STLCLR.dll*,
                                // sometimes when they are specified as linker arguments libs
                                // will use SCREAMING_SNAKE_CASE as if they are angry at the
                                // linker this list is probably not completely, but that's
                                // what PRs are for
                                if let Some(angry_lib) = match fnamestr.strip_suffix(".lib") {
                                    Some("libcmt") => Some("LIBCMT.lib"),
                                    Some("msvcrt") => Some("MSVCRT.lib"),
                                    Some("oldnames") => Some("OLDNAMES.lib"),
                                    _ => None,
                                } {
                                    tar.pop();
                                    tar.push(angry_lib);

                                    symlink(fnamestr, &tar)?;
                                }
                            }
                            PayloadKind::SdkHeaders => {
                                // The SDK headers are again all over the place with casing
                                // as well as being internally inconsistent, so we scan
                                // them all for includes and add those that are referenced
                                // incorrectly, but we wait until after all the of headers
                                // have been unpacked before fixing them
                            }
                            PayloadKind::SdkLibs | PayloadKind::SdkStoreLibs => {
                                // The SDK libraries are just completely inconsistent, but
                                // all usage I have ever seen just links them with lowercase
                                // names, so we just fix all of them to be lowercase.
                                // Note that we need to not only fix the name but also the
                                // extension, as for some inexplicable reason about half of
                                // them use an uppercase L for the extension. WTF. This also
                                // applies to the tlb files, so at least they are consistently
                                // inconsistent
                                if fnamestr.contains(|c: char| c.is_ascii_uppercase()) {
                                    tar.pop();
                                    tar.push(fnamestr.to_ascii_lowercase());

                                    symlink(fnamestr, &tar)?;
                                }
                            }
                        }
                    }

                    tar.pop();
                }

                // Due to some libs from the CRT Store libs variant being needed
                // by the regular Desktop variant, if we are not actually
                // targetting the Store we can avoid adding the additional
                // uwp and store subdirectories
                if mapping.kind == PayloadKind::CrtLibs
                    && mapping.variant == Some(Variant::Store)
                    && (variants & Variant::Store as u32) == 0
                {
                    tracing::debug!("skipping CRT subdirs");

                    item.progress
                        .inc(tree.dirs.iter().map(|(_, ft)| ft.stats().1).sum());
                } else {
                    for (dir, dtree) in &tree.dirs {
                        dir_stack.push(Dir {
                            src: src.join(dir),
                            tar: tar.join(dir),
                            tree: dtree,
                        });
                    }
                }
            }

            Ok(())
        })
        .collect_into_vec(&mut results);

    item.progress.finish_with_message("📦 splatted");

    Ok(())
}

#[inline]
fn symlink(original: &str, link: &Path) -> Result<(), Error> {
    std::os::unix::fs::symlink(original, link)
        .with_context(|| format!("unable to symlink from {} to {}", link, original))
}

pub(crate) fn finalize_splat(
    roots: &SplatRoots,
    sdk_files: Arc<Mutex<HashMap<u64, PathBuf>>>,
) -> Result<(), Error> {
    let files = std::sync::Arc::try_unwrap(sdk_files).unwrap().into_inner();
    let mut includes: std::collections::HashSet<
        _,
        std::hash::BuildHasherDefault<twox_hash::XxHash64>,
    > = Default::default();

    // Many headers won't necessarily be referenced internally by an all
    // lower case filename, even when that is common from outside the sdk
    // for basically all files (eg windows.h, psapi.h etc)
    includes.extend(files.values().filter_map(|fpath| {
        fpath.file_name().and_then(|fname| {
            fname
                .contains(|c: char| c.is_ascii_uppercase())
                .then(|| fname.to_ascii_lowercase())
        })
    }));

    let regex = regex::bytes::Regex::new(r#"#include\s+(?:"|<)([^">]+)(?:"|>)?"#).unwrap();

    let pb = indicatif::ProgressBar::with_draw_target(
        files.len() as u64,
        indicatif::ProgressDrawTarget::stdout(),
    )
    .with_style(
        indicatif::ProgressStyle::default_bar()
            .template("{spinner:.green} {prefix:.bold} [{elapsed}] {wide_bar:.green} {pos}/{len}")
            .progress_chars("█▇▆▅▄▃▂▁  "),
    );

    pb.set_prefix("symlinks");
    pb.set_message("🔍 includes");

    // Scan all of the files in the include directory for includes so that
    // we can add symlinks to at least make the SDK headers internally consistent
    for file in files.values() {
        // Of course, there are files with non-utf8 encoding :p
        let contents = std::fs::read(file).with_context(|| format!("unable to read {}", file))?;

        for caps in regex.captures_iter(&contents) {
            let name = std::str::from_utf8(&caps[1]).with_context(|| {
                format!("{} contained an include with non-utf8 characters", file)
            })?;

            let name = match name.rfind('/') {
                Some(i) => &name[i + 1..],
                None => name,
            };

            if !includes.contains(name) {
                includes.insert(name.to_owned());
            }
        }

        pb.inc(1);
    }

    pb.finish();

    for include in includes {
        let lower_hash = calc_lower_hash(&include);

        match files.get(&lower_hash) {
            Some(disk_name) => {
                if let Some(fname) = disk_name.file_name() {
                    if fname != include {
                        let mut link = disk_name.clone();
                        link.pop();
                        link.push(include);
                        symlink(fname, &link)?;
                    }
                }
            }
            None => {
                tracing::debug!(
                    "SDK include for '{}' was not found in the SDK headers",
                    include
                );
            }
        }
    }

    // There is a um/gl directory, but of course there is an include for GL/
    // instead, so fix that as well :p
    symlink("gl", &roots.sdk.join("include/um/GL"))?;

    Ok(())
}

#[inline]
fn calc_lower_hash(path: &str) -> u64 {
    use std::hash::Hasher;
    let mut hasher = twox_hash::XxHash64::with_seed(0);

    for c in path.chars().map(|c| c.to_ascii_lowercase() as u8) {
        hasher.write_u8(c);
    }

    hasher.finish()
}
