// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! fw_cfg boot-order generation from PCI device specifications.

use std::collections::BTreeMap;

use anyhow::{ensure, Context};

const LEGACY_BOOTORDER: &[u8] = b"/pci@i0cf8/pci@4,0\n";

/// Remove any `bootindex=N` token from a `-s` spec, returning the
/// cleaned spec and the parsed index.
///
/// # Errors
///
/// Returns an error if the index is not an integer in `1..=255`, or if the
/// specification contains more than one `bootindex` token.
pub fn strip_bootindex(spec: &str) -> anyhow::Result<(String, Option<u8>)> {
    let mut fields = Vec::new();
    let mut bootindex = None;

    for field in spec.split(',') {
        let Some(raw_index) = field.strip_prefix("bootindex=") else {
            fields.push(field);
            continue;
        };

        ensure!(
            bootindex.is_none(),
            "multiple bootindex values in PCI slot spec '{spec}'",
        );
        let index = raw_index.parse::<u8>().with_context(|| {
            format!("invalid bootindex '{raw_index}' in PCI slot spec '{spec}'")
        })?;
        ensure!(
            index != 0,
            "bootindex must be in 1..=255 in PCI slot spec '{spec}'",
        );
        bootindex = Some(index);
    }

    Ok((fields.join(","), bootindex))
}

/// Build the fw_cfg `bootorder` payload from the `-s` specs.
///
/// # Errors
///
/// Returns an error if a boot index is invalid or duplicated, or if the PCI
/// address in an indexed specification is invalid.
pub fn build_bootorder(specs: &[String]) -> anyhow::Result<Vec<u8>> {
    let mut indexed = BTreeMap::new();

    for spec in specs {
        let (cleaned_spec, bootindex) = strip_bootindex(spec)?;
        let Some(index) = bootindex else {
            continue;
        };
        let bdf_field = cleaned_spec.split(',').next().unwrap_or("");
        let bdf = vmm_machine::parse_bdf(bdf_field).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid BDF '{bdf_field}' in PCI slot spec '{spec}'"
            )
        })?;

        if let Some((first_spec, _)) = indexed.get(&index) {
            anyhow::bail!(
                "duplicate bootindex {index} in PCI slot specs '{first_spec}' and '{spec}'",
            );
        }
        indexed.insert(index, (spec, bdf));
    }

    if indexed.is_empty() {
        return Ok(LEGACY_BOOTORDER.to_vec());
    }

    let mut payload = Vec::new();
    for (_, (_, bdf)) in indexed {
        let entry = format!("/pci@i0cf8/pci@{},{}\n", bdf.dev(), bdf.func());
        payload.extend_from_slice(entry.as_bytes());
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn no_bootindex_matches_legacy_string() {
        let result = build_bootorder(&specs(&["4,nvme,/disk"])).unwrap();
        assert_eq!(result, b"/pci@i0cf8/pci@4,0\n");
    }

    #[test]
    fn single_bootindex_emits_that_slot() {
        let result =
            build_bootorder(&specs(&["5,nvme,/iso,bootindex=1"])).unwrap();
        assert_eq!(result, b"/pci@i0cf8/pci@5,0\n");
    }

    #[test]
    fn two_devices_ordered_by_index() {
        let result = build_bootorder(&specs(&[
            "5,nvme,/iso,ro,bootindex=1",
            "4,nvme,/disk,bootindex=2",
        ]))
        .unwrap();
        assert_eq!(result, b"/pci@i0cf8/pci@5,0\n/pci@i0cf8/pci@4,0\n",);
    }

    #[test]
    fn duplicate_bootindex_is_error() {
        let result = build_bootorder(&specs(&[
            "5,nvme,/iso,bootindex=1",
            "4,nvme,/disk,bootindex=1",
        ]));
        let error = result.unwrap_err().to_string();
        assert!(error.contains("5,nvme,/iso,bootindex=1"));
        assert!(error.contains("4,nvme,/disk,bootindex=1"));
    }

    #[test]
    fn bootindex_zero_is_error() {
        assert!(strip_bootindex("4,nvme,/disk,bootindex=0").is_err());
    }

    #[test]
    fn bootindex_non_numeric_is_error() {
        assert!(strip_bootindex("4,nvme,/disk,bootindex=first").is_err());
    }

    #[test]
    fn bootindex_out_of_range_is_error() {
        assert!(strip_bootindex("4,nvme,/disk,bootindex=256").is_err());
    }

    #[test]
    fn two_bootindex_tokens_in_one_spec_is_error() {
        assert!(
            strip_bootindex("4,nvme,/disk,bootindex=1,bootindex=2").is_err()
        );
    }

    #[test]
    fn bootindex_is_stripped_from_device_config() {
        let (cleaned, index) =
            strip_bootindex("4,nvme,/disk,bootindex=2").unwrap();
        assert_eq!(cleaned, "4,nvme,/disk");
        assert_eq!(index, Some(2));
    }

    #[test]
    fn bootindex_stripped_mid_config() {
        let (cleaned, index) =
            strip_bootindex("5,nvme,/iso,bootindex=1,ro").unwrap();
        assert_eq!(cleaned, "5,nvme,/iso,ro");
        assert_eq!(index, Some(1));
    }

    #[test]
    fn spec_without_bootindex_is_unchanged() {
        let original = "5,nvme,/iso,ro";
        let (cleaned, index) = strip_bootindex(original).unwrap();
        assert_eq!(cleaned, original);
        assert_eq!(index, None);
    }

    #[test]
    fn bootindex_with_explicit_func() {
        let result =
            build_bootorder(&specs(&["0:5:1,nvme,/iso,bootindex=1"])).unwrap();
        assert_eq!(result, b"/pci@i0cf8/pci@5,1\n");
    }
}
