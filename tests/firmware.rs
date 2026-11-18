use esp32c3_flashtool::{
    chip::ChipModel,
    firmware::{FirmwareInput, PreparedFirmware, prepare_firmware},
};
use std::{collections::HashSet, fs};
use tempfile::TempDir;

fn bin(dir: &TempDir, name: &str, address: u32, size: usize, chip: Option<u16>) -> FirmwareInput {
    let path = dir.path().join(name);
    let mut data = vec![0xa5; size];
    if let Some(id) = chip {
        data[0] = 0xe9;
        data[12..14].copy_from_slice(&id.to_le_bytes());
    }
    fs::write(&path, data).unwrap();
    FirmwareInput { path, address }
}

fn c3(inputs: &[FirmwareInput]) -> anyhow::Result<PreparedFirmware> {
    prepare_firmware(inputs, ChipModel::Esp32c3)
}

#[test]
fn merged_c3_and_snapshot_are_immutable() {
    let d = TempDir::new().unwrap();
    let input = bin(&d, "merged.bin", 0, 4101, Some(5));
    let fw = c3(std::slice::from_ref(&input)).unwrap();
    assert_eq!(fw.total_bytes, 4101);
    assert_eq!(fw.chip, ChipModel::Esp32c3);
    assert_eq!(fw.segments[0].data.len(), 4104);
    assert_eq!(&fw.segments[0].data[4101..], &[0xff; 3]);
    fs::write(input.path, b"changed after queue started").unwrap();
    assert_eq!(fw.segments[0].data[0], 0xe9);
}

#[test]
fn c3_split_layout_and_digest_are_order_independent() {
    let d = TempDir::new().unwrap();
    let a = bin(&d, "bootloader.bin", 0, 20528, Some(5));
    let b = bin(&d, "partitions.bin", 0x8000, 3072, None);
    let c = bin(&d, "app.bin", 0x10000, 64 * 1024, Some(5));
    let fw = c3(&[a.clone(), b.clone(), c.clone()]).unwrap();
    let reordered = c3(&[c, b, a]).unwrap();
    assert_eq!(fw.sha256, reordered.sha256);
    assert_eq!(fw.segments.len(), 3);
}

#[test]
fn rejects_wrong_chip_and_missing_boot_header() {
    let d = TempDir::new().unwrap();
    assert!(c3(&[bin(&d, "s3.bin", 0, 64, Some(9))]).is_err());
    assert!(c3(&[bin(&d, "raw.bin", 0, 64, None)]).is_err());
}

#[test]
fn rejects_sector_overlap_duplicate_addresses_and_unaligned_offsets() {
    let d = TempDir::new().unwrap();
    let a = bin(&d, "a.bin", 0, 4097, Some(5));
    let b = bin(&d, "b.bin", 0x1000, 64, None);
    assert!(c3(&[a, b]).unwrap_err().to_string().contains("重叠"));
    let a = bin(&d, "a.bin", 0x8000, 64, None);
    let b = bin(&d, "b.bin", 0x8000, 64, None);
    assert!(c3(&[a, b]).is_err());
    assert!(c3(&[bin(&d, "c.bin", 0x1001, 64, None)]).is_err());
}

#[test]
fn rejects_empty_oversized_and_out_of_range_firmware() {
    let d = TempDir::new().unwrap();
    assert!(c3(&[]).is_err());
    assert!(c3(&[bin(&d, "empty.bin", 0x8000, 0, None)]).is_err());
    let large = bin(&d, "large.bin", 0, 16 * 1024 * 1024 + 1, Some(5));
    assert!(c3(&[large]).is_err());
    assert!(c3(&[bin(&d, "out.bin", 0x1000000, 64, None)]).is_err());
}

#[test]
fn layout_address_is_part_of_firmware_identity() {
    let d = TempDir::new().unwrap();
    let a = bin(&d, "data.bin", 0x8000, 64, None);
    let mut b = a.clone();
    b.address = 0x9000;
    assert_ne!(c3(&[a]).unwrap().sha256, c3(&[b]).unwrap().sha256);
}

#[test]
fn every_supported_chip_accepts_its_bootloader_at_the_correct_offset() {
    let d = TempDir::new().unwrap();
    for chip in ChipModel::all() {
        let input = bin(
            &d,
            &format!("{}.bin", chip.slug()),
            chip.bootloader_address(),
            64,
            Some(chip.image_chip_id()),
        );
        let fw = prepare_firmware(&[input], *chip).unwrap();
        assert_eq!(fw.chip, *chip);
        assert_eq!(fw.segments[0].address, chip.bootloader_address());
    }
}

#[test]
fn selected_chip_is_checked_for_every_bootloader_and_application_image() {
    let d = TempDir::new().unwrap();
    for chip in ChipModel::all() {
        let wrong = if *chip == ChipModel::Esp32c3 {
            ChipModel::Esp32s3
        } else {
            ChipModel::Esp32c3
        };
        for address in [chip.bootloader_address(), 0x10000] {
            let input = bin(&d, "wrong.bin", address, 64, Some(wrong.image_chip_id()));
            assert!(
                prepare_firmware(&[input], *chip).is_err(),
                "{chip} must reject {wrong} image"
            );
        }
        let raw = bin(&d, "raw.bin", chip.bootloader_address(), 64, None);
        assert!(prepare_firmware(&[raw], *chip).is_err());
    }
}

#[test]
fn esp32_and_s2_merged_images_require_erased_prefix_and_correct_embedded_header() {
    let d = TempDir::new().unwrap();
    for chip in [ChipModel::Esp32, ChipModel::Esp32s2] {
        let mut bytes = vec![0xff; 0x1000 + 64];
        bytes[0x1000] = 0xe9;
        bytes[0x100c..0x100e].copy_from_slice(&chip.image_chip_id().to_le_bytes());
        let path = d.path().join("merged.bin");
        let input = FirmwareInput {
            path: path.clone(),
            address: 0,
        };
        fs::write(&path, &bytes).unwrap();
        let fw = prepare_firmware(std::slice::from_ref(&input), chip).unwrap();
        assert_eq!(fw.segments[0].address, 0);
        assert_eq!(fw.segments[0].data, bytes);

        bytes[0x100c..0x100e].copy_from_slice(&5u16.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        assert!(prepare_firmware(std::slice::from_ref(&input), chip).is_err());
        bytes[0x100c..0x100e].copy_from_slice(&chip.image_chip_id().to_le_bytes());
        bytes[0] = 0;
        fs::write(&path, &bytes).unwrap();
        assert!(prepare_firmware(std::slice::from_ref(&input), chip).is_err());
        fs::write(&path, vec![0xff; 0x1000 + 23]).unwrap();
        assert!(prepare_firmware(&[input], chip).is_err());

        // A bare bootloader at 0x0 must not be accepted as a merged image.
        let bare = bin(&d, "bare.bin", 0, 64, Some(chip.image_chip_id()));
        assert!(prepare_firmware(&[bare], chip).is_err());
    }
}

#[test]
fn raw_regions_still_work_and_selected_chip_is_part_of_layout_identity() {
    let d = TempDir::new().unwrap();
    let input = bin(&d, "nvs.bin", 0x9000, 64, None);
    let hashes: HashSet<_> = ChipModel::all()
        .iter()
        .map(|chip| {
            prepare_firmware(std::slice::from_ref(&input), *chip)
                .unwrap()
                .sha256
        })
        .collect();
    assert_eq!(hashes.len(), ChipModel::all().len());
}
