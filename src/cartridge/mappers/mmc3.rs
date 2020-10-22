use cartridge::mirroring::MirroringMode;
use cartridge::CartridgeHeader;
use cartridge::CpuCartridgeAddressBus;
use cartridge::PpuCartridgeAddressBus;
use log::{debug, error, info};

#[derive(Debug)]
enum PRGBankMode {
    /// 8000-9FFF swappable bank, C000-DFFF fixed to second last bank
    LowBankSwappable,
    /// C000-DFFF swappable bank, 8000-9FFF fixed to second last bank
    HighBankSwappable,
}

pub(crate) struct MMC3PrgChip {
    prg_rom: Vec<u8>,
    total_prg_banks: u8,
    prg_ram: Option<[u8; 0x2000]>,
    prg_banks: [u8; 4],
    prg_bank_offsets: [usize; 4],
    bank_mode: PRGBankMode,
    /// 0b000-0b111 -> The register to be written to on next write to BankData
    bank_select: u8,
}

impl MMC3PrgChip {
    fn new(prg_rom: Vec<u8>, total_prg_banks: u8, prg_ram: Option<[u8; 0x2000]>) -> Self {
        debug_assert!(prg_rom.len() >= 0x4000);

        MMC3PrgChip {
            prg_rom,
            total_prg_banks,
            prg_ram,
            prg_banks: [0, 1, total_prg_banks - 2, total_prg_banks - 1],
            prg_bank_offsets: [
                0x0000,
                0x2000,
                (total_prg_banks as usize - 2) * 0x2000,
                (total_prg_banks as usize - 1) * 0x2000,
            ],
            bank_mode: PRGBankMode::LowBankSwappable,
            bank_select: 0, // TODO - Does this initial value matter?
        }
    }

    fn update_bank_offsets(&mut self) {
        match self.bank_mode {
            PRGBankMode::LowBankSwappable => {
                self.prg_bank_offsets[0] = self.prg_banks[0] as usize * 0x2000;
            }
            PRGBankMode::HighBankSwappable => {
                self.prg_bank_offsets[2] = self.prg_banks[0] as usize * 0x2000;
            }
        };

        info!(
            "MMC3 PRG bank offsets updated {:?} -> {:?}",
            self.prg_banks, self.prg_bank_offsets
        );
    }
}

impl CpuCartridgeAddressBus for MMC3PrgChip {
    fn read_byte(&self, address: u16) -> u8 {
        match address {
            0x6000..=0x7FFF => match self.prg_ram {
                Some(ram) => ram[(address - 0x6000) as usize],
                None => 0x0,
            },
            // PRG Bank 0 - Switchable or fixed to second to last bank
            0x8000..=0x9FFF => {
                let adj_addr = address as usize - 0x8000;
                self.prg_rom[adj_addr + self.prg_bank_offsets[0] as usize]
            }
            // PRG Bank 1 - Switchable
            0xA000..=0xBFFF => {
                let adj_addr = address as usize - 0x8000;
                self.prg_rom[adj_addr + self.prg_bank_offsets[1] as usize]
            }
            // PRG Bank 2 - Switchable or fixed to second to last bank (swaps with bank 0)
            0xC000..=0xDFFF => {
                let adj_addr = address as usize - 0xC000;
                self.prg_rom[adj_addr + self.prg_bank_offsets[2] as usize]
            }
            // PRG Bank 3 - Fixed to last bank
            0xE000..=0xFFFF => {
                let adj_addr = address as usize - 0xE000;
                self.prg_rom[adj_addr + self.prg_bank_offsets[3] as usize]
            }
            _ => 0x0, // TODO - Would like to understand what reads of 0x4025 e.g. do here.
        }
    }

    fn write_byte(&mut self, address: u16, value: u8, _: u32) {
        match address {
            0x6000..=0x7FFF => {
                if let Some(mut ram) = self.prg_ram {
                    ram[(address - 0x6000) as usize] = value
                }
            }
            // Bank select and Bank data registers
            0x8000..=0x9FFF => match address & 1 {
                // Even addresses => Bank select register
                0 => {
                    self.bank_select = value & 0b0000_0111;
                    self.bank_mode = if value & 0b0100_0000 == 0 {
                        PRGBankMode::LowBankSwappable
                    } else {
                        PRGBankMode::HighBankSwappable
                    };

                    self.update_bank_offsets();
                }
                1 => {
                    match self.bank_select {
                        0b110 => self.prg_banks[0] = value % self.total_prg_banks,
                        0b111 => self.prg_banks[3] = value % self.total_prg_banks,
                        _ => (), // Do nothing with CHR registers here
                    };

                    self.update_bank_offsets();
                }
                _ => panic!(),
            },
            // Mirroring & PRG RAM Protect registers
            0xA000..=0xBFFF => {}
            // IRQ Latch & IRQ Reload registers - TODO - Implement IRQ counter
            0xC000..=0xDFFF => {}
            // IRQ Disable/Enable registers - TODO - Implement IRQ counter
            0xE000..=0xFFFF => {}
            _ => (),
        }
    }
}

#[derive(Debug)]
enum CHRBankMode {
    /// Two 2KB banks at 0000-0FFF and four 1KB banks at 1000-1FFF  
    LowBank2KB,
    /// Two 2KB banks at 1000-1FFF and four 1KB banks at 0000-0FFF
    HighBank2KB,
}

pub(crate) struct MMC3ChrChip {
    chr_data: Vec<u8>,
    total_chr_banks: u8,
    ppu_vram: [u8; 0x1000],
    chr_banks: [u8; 8],
    chr_bank_offsets: [usize; 8],
    mirroring_mode: MirroringMode,
    bank_mode: CHRBankMode,
    /// 0b000-0b111 -> The register to be written to on next write to BankData
    bank_select: u8,
}

impl MMC3ChrChip {
    fn new(chr_rom: Vec<u8>, banks: u8, mirroring_mode: MirroringMode) -> Self {
        MMC3ChrChip {
            chr_data: chr_rom,
            total_chr_banks: banks,
            ppu_vram: [0; 0x1000],
            chr_banks: [0, 1, 2, 3, 4, 5, 6, 7],
            chr_bank_offsets: [0x0000, 0x0400, 0x0800, 0x0C00, 0x1000, 0x1400, 0x1800, 0x1C00],
            mirroring_mode,
            bank_mode: CHRBankMode::LowBank2KB,
            bank_select: 0,
        }
    }

    fn update_bank_offsets(&mut self) {
        match self.bank_mode {
            CHRBankMode::LowBank2KB => {
                for i in 0..8 {
                    self.chr_bank_offsets[i] = self.chr_banks[i] as usize * 0x400;
                }
            }
            CHRBankMode::HighBank2KB => {
                for i in 0..8 {
                    self.chr_bank_offsets[(i + 4) % 8] = self.chr_banks[i] as usize * 0x400;
                }
            }
        };

        info!(
            "MMC3 CHR bank offsets updated {:?} -> {:?}",
            self.chr_banks, self.chr_bank_offsets
        );
    }
}

impl PpuCartridgeAddressBus for MMC3ChrChip {
    fn read_byte(&self, address: u16) -> u8 {
        match address {
            0x0000..=0x03FF => self.chr_data[address as usize - 0x0000 + self.chr_bank_offsets[0]],
            0x0400..=0x07FF => self.chr_data[address as usize - 0x0400 + self.chr_bank_offsets[1]],
            0x0800..=0x0BFF => self.chr_data[address as usize - 0x0800 + self.chr_bank_offsets[2]],
            0x0C00..=0x0FFF => self.chr_data[address as usize - 0x0C00 + self.chr_bank_offsets[3]],
            0x1000..=0x13FF => self.chr_data[address as usize - 0x1000 + self.chr_bank_offsets[4]],
            0x1400..=0x17FF => self.chr_data[address as usize - 0x1400 + self.chr_bank_offsets[5]],
            0x1800..=0x1BFF => self.chr_data[address as usize - 0x1800 + self.chr_bank_offsets[6]],
            0x1C00..=0x1FFF => self.chr_data[address as usize - 0x1C00 + self.chr_bank_offsets[7]],
            0x2000..=0x3EFF => {
                let mirrored_address = self.mirroring_mode.get_mirrored_address(address);
                debug!("Read {:04X} mirrored to {:04X}", address, mirrored_address);

                self.ppu_vram[mirrored_address as usize]
            }
            0x3F00..=0x3FFF => panic!("Shouldn't be reading from palette RAM through cartridge bus"),
            _ => panic!("Reading from {:04X} invalid for CHR address bus", address),
        }
    }

    fn write_byte(&mut self, address: u16, value: u8, _: u32) {
        debug!("MMC3 CHR write {:04X}={:02X}", address, value);

        match address {
            0x0000..=0x1FFF => (),
            0x2000..=0x3EFF => {
                let mirrored_address = self.mirroring_mode.get_mirrored_address(address);

                self.ppu_vram[mirrored_address as usize] = value;
            }
            0x3F00..=0x3FFF => panic!("Shouldn't be writing to palette registers through the cartridge address bus"),
            _ => panic!("Write to {:04X} ({:02X}) invalid for CHR address bus", address, value),
        }
    }

    fn cpu_write_byte(&mut self, address: u16, value: u8, _: u32) {
        debug!("CPU write to MMC3 CHR bus {:04X}={:02X}", address, value);

        match address {
            // Bank select and Bank data registers
            0x8000..=0x9FFF => match address & 1 {
                // Even addresses => Bank select register
                0 => {
                    self.bank_select = value & 0b0000_0111;
                    self.bank_mode = if value & 0b1000_0000 == 0 {
                        CHRBankMode::LowBank2KB
                    } else {
                        CHRBankMode::HighBank2KB
                    };

                    self.update_bank_offsets();
                }
                1 => {
                    match self.bank_select {
                        0b000 => {
                            self.chr_banks[0] = (value & 0b1111_1110) % self.total_chr_banks;
                            self.chr_banks[1] = self.chr_banks[0] + 1;
                        }
                        0b001 => {
                            self.chr_banks[2] = (value & 0b1111_1110) % self.total_chr_banks;
                            self.chr_banks[3] = self.chr_banks[2] + 1;
                        }
                        0b010..=0b101 => self.chr_banks[self.bank_select as usize + 2] = value % self.total_chr_banks,
                        _ => (), // Do nothing with PRG banks here
                    };

                    self.update_bank_offsets();
                }
                _ => panic!(),
            },
            // Mirroring & PRG RAM Protect registers - PRG RAM handled by PRG cartridge
            0xA000..=0xBFFF => {
                if address & 1 == 0 && self.mirroring_mode != MirroringMode::FourScreen {
                    self.mirroring_mode = if value & 1 == 0 {
                        MirroringMode::Vertical
                    } else {
                        MirroringMode::Horizontal
                    };

                    info!("MMC3 mirroring mode change {:?}", self.mirroring_mode);
                }
            }
            // IRQ Latch & IRQ Reload registers - TODO - Implement IRQ counter
            0xC000..=0xDFFF => {}
            // IRQ Disable/Enable registers - TODO - Implement IRQ counter
            0xE000..=0xFFFF => {}
            _ => (),
        }
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
    (
        Box::new(MMC3PrgChip::new(
            prg_rom,
            header.prg_rom_16kb_units * 2,
            Some([0; 0x2000]),
        )),
        Box::new(MMC3ChrChip::new(
            chr_rom.unwrap(),
            header.chr_rom_8kb_units * 2,
            header.mirroring,
        )),
        header,
    )
}