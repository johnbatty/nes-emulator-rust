use cartridge::mappers::{BankedChrChip, BankedPrgChip};
use cartridge::CartridgeHeader;
use cartridge::CpuCartridgeAddressBus;
use cartridge::PpuCartridgeAddressBus;
use log::info;

fn uxrom_update_banks(address: u16, value: u8, total_banks: u8, banks: &mut [u8; 2], bank_offsets: &mut [usize; 2]) {
    if let 0x8000..=0xFFFF = address {
        banks[0] = value % total_banks;
        bank_offsets[0] = banks[0] as usize * 0x4000;
        info!("Bank switch {:?} -> {:?}", banks, bank_offsets);
    }
}

pub(crate) fn from_header(
    prg_rom: Vec<u8>,
    chr_rom: Option<Vec<u8>>,
    header: CartridgeHeader,
) -> (
    Box<dyn CpuCartridgeAddressBus>,
    Box<dyn PpuCartridgeAddressBus>,
    CartridgeHeader,
) {
    info!("Creating UxROM mapper for cartridge {:?}", header);
    (
        Box::new(BankedPrgChip::new(
            prg_rom,
            None,
            header.prg_rom_16kb_units,
            [0, header.prg_rom_16kb_units - 1],
            [0, (header.prg_rom_16kb_units as usize - 1) * 0x4000],
            uxrom_update_banks,
        )),
        Box::new(BankedChrChip::new(chr_rom, header.mirroring, 1)),
        header,
    )
}