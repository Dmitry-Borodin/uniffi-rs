/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

/// Alternative implementation for the `generate` command, that we plan to eventually replace the current default with.
///
/// Traditionally, users would invoke `uniffi-bindgen generate` to generate bindings for a single crate, passing it the UDL file, config file, etc.
///
/// library_mode is a new way to generate bindings for multiple crates at once.
/// Users pass the path to the build cdylib file and UniFFI figures everything out, leveraging `cargo_metadata`, the metadata UniFFI stores inside exported symbols in the dylib, etc.
///
/// This brings several advantages.:
///   - No more need to specify the dylib in the `uniffi.toml` file(s)
///   - UniFFI can figure out the dependencies based on the dylib exports and generate the sources for
///     all of them at once.
///   - UniFFI can figure out the package/module names for each crate, eliminating the external
///     package maps.
use crate::{
    bindings::{self, TargetLanguage},
    macro_metadata, ComponentInterface, Config, Result,
};
use anyhow::{bail, Context};
use camino::Utf8Path;
use cargo_metadata::{MetadataCommand, Package};
use std::{
    collections::{HashMap, HashSet},
    fs,
};
use uniffi_meta::{group_metadata, MetadataGroup};

/// Generate foreign bindings
///
/// Returns the list of sources used to generate the bindings, in no particular order.
pub fn generate_bindings(
    library_path: &Utf8Path,
    crate_name: Option<String>,
    target_languages: &[TargetLanguage],
    out_dir: &Utf8Path,
    try_format_code: bool,
) -> Result<Vec<Source>> {
    let cargo_metadata = MetadataCommand::new()
        .exec()
        .context("error running cargo metadata")?;
    let cdylib_name = calc_cdylib_name(library_path);
    let mut sources = find_sources(&cargo_metadata, library_path, cdylib_name)?;
    for i in 0..sources.len() {
        // Partition up the sources list because we're eventually going to call
        // `update_from_dependency_configs()` which requires an exclusive reference to one source and
        // shared references to all other sources.
        let (sources_before, rest) = sources.split_at_mut(i);
        let (source, sources_after) = rest.split_first_mut().unwrap();
        let other_sources = sources_before.iter().chain(sources_after.iter());
        // Calculate which configs come from dependent crates
        let dependencies =
            HashSet::<&str>::from_iter(source.package.dependencies.iter().map(|d| d.name.as_str()));
        let config_map: HashMap<&str, &Config> = other_sources
            .filter_map(|s| {
                dependencies
                    .contains(s.package.name.as_str())
                    .then_some((s.crate_name.as_str(), &s.config))
            })
            .collect();
        // We can finally call update_from_dependency_configs
        source.config.update_from_dependency_configs(config_map);
    }
    fs::create_dir_all(out_dir)?;
    if let Some(crate_name) = &crate_name {
        let old_elements = sources.drain(..);
        let mut matches: Vec<_> = old_elements
            .filter(|s| &s.crate_name == crate_name)
            .collect();
        match matches.len() {
            0 => bail!("Crate {crate_name} not found in {library_path}"),
            1 => sources.push(matches.pop().unwrap()),
            n => bail!("{n} crates named {crate_name} found in {library_path}"),
        }
    }

    for source in sources.iter() {
        for &language in target_languages {
            if cdylib_name.is_none() && language != TargetLanguage::Swift {
                bail!("Generate bindings for {language} requires a cdylib, but {library_path} was given");
            }
            bindings::write_bindings(
                &source.config.bindings,
                &source.ci,
                out_dir,
                language,
                try_format_code,
            )?;
        }
    }

    Ok(sources)
}

// A single source that we generate bindings for
#[derive(Debug)]
pub struct Source {
    pub package: Package,
    pub crate_name: String,
    pub ci: ComponentInterface,
    pub config: Config,
}

// If `library_path` is a C dynamic library, return its name
pub fn calc_cdylib_name(library_path: &Utf8Path) -> Option<&str> {
    let cdylib_extentions = [".so", ".dll", ".dylib"];
    let filename = library_path.file_name()?;
    let filename = filename.strip_prefix("lib").unwrap_or(filename);
    for ext in cdylib_extentions {
        if let Some(f) = filename.strip_suffix(ext) {
            return Some(f);
        }
    }
    None
}

fn find_sources(
    cargo_metadata: &cargo_metadata::Metadata,
    library_path: &Utf8Path,
    cdylib_name: Option<&str>,
) -> Result<Vec<Source>> {
    group_metadata(macro_metadata::extract_from_library(library_path)?)?
        .into_iter()
        .map(|group| {
            let package = find_package_by_crate_name(cargo_metadata, &group.namespace.crate_name)?;
            let crate_root = package
                .manifest_path
                .parent()
                .context("manifest path has no parent")?;
            let crate_name = group.namespace.crate_name.clone();
            let mut ci = ComponentInterface::default();
            if let Some(metadata) = load_udl_metadata(&group, crate_root, &crate_name)? {
                ci.add_metadata(metadata)?;
            };
            ci.add_metadata(group)?;
            let mut config = Config::load_initial(crate_root, None)?;
            if let Some(cdylib_name) = cdylib_name {
                config.update_from_cdylib_name(cdylib_name);
            }
            config.update_from_ci(&ci);
            Ok(Source {
                config,
                crate_name,
                ci,
                package,
            })
        })
        .collect()
}

fn find_package_by_crate_name(
    metadata: &cargo_metadata::Metadata,
    crate_name: &str,
) -> Result<Package> {
    let matching: Vec<&Package> = metadata
        .packages
        .iter()
        .filter(|p| {
            p.targets
                .iter()
                .any(|t| t.name.replace('-', "_") == crate_name)
        })
        .collect();
    match matching.len() {
        1 => Ok(matching[0].clone()),
        n => bail!("cargo metadata returned {n} packages for crate name {crate_name}"),
    }
}

fn load_udl_metadata(
    group: &MetadataGroup,
    crate_root: &Utf8Path,
    crate_name: &str,
) -> Result<Option<MetadataGroup>> {
    let udl_items = group
        .items
        .iter()
        .filter_map(|i| match i {
            uniffi_meta::Metadata::UdlFile(meta) => Some(meta),
            _ => None,
        })
        .collect::<Vec<_>>();
    match udl_items.len() {
        // No UDL files, load directly from the group
        0 => Ok(None),
        // Found a UDL file, use it to load the CI, then add the MetadataGroup
        1 => {
            let ci_name = &udl_items[0].name;
            let ci_path = crate_root.join("src").join(format!("{ci_name}.udl"));
            if ci_path.exists() {
                let udl = fs::read_to_string(ci_path)?;
                Ok(Some(uniffi_udl::parse_udl(&udl)?))
            } else {
                bail!("{ci_path} not found");
            }
        }
        n => bail!("{n} UDL files found for {crate_name}"),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn calc_cdylib_name_is_correct() {
        assert_eq!(
            "uniffi",
            calc_cdylib_name("/path/to/libuniffi.so".into()).unwrap()
        );
        assert_eq!(
            "uniffi",
            calc_cdylib_name("/path/to/libuniffi.dylib".into()).unwrap()
        );
        assert_eq!(
            "uniffi",
            calc_cdylib_name("/path/to/uniffi.dll".into()).unwrap()
        );
    }

    /// Right now we unconditionally strip the `lib` prefix.
    ///
    /// Technically Windows DLLs do not start with a `lib` prefix,
    /// but a library name could start with a `lib` prefix.
    /// On Linux/macOS this would result in a `liblibuniffi.{so,dylib}` file.
    #[test]
    #[ignore] // Currently fails.
    fn calc_cdylib_name_is_correct_on_windows() {
        assert_eq!(
            "libuniffi",
            calc_cdylib_name("/path/to/libuniffi.dll".into()).unwrap()
        );
    }
}
