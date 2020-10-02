mod opcodes;
mod registers;
mod status_flags;

use cpu::opcodes::Opcode;
use cpu::opcodes::{AddressingMode, InstructionType, Operation, OPCODE_TABLE};
use cpu::registers::Registers;
use cpu::status_flags::StatusFlags;
use log::{debug, error, info};
use mmu::Mmu;

///
/// Cpu states are used to represent cycles of an instruction
///
#[derive(Debug, Copy, Clone)]
enum CpuState {
    // Cycle 1 is always reading the PC and incrementing it
    FetchOpcode,
    // Cycle 2 always reads the (incremented) PC, but for implied &
    // accumulator modes this value is then discarded and the PC is not
    // incremented
    ThrowawayRead {
        opcode: &'static Opcode,
        operand: Option<u8>,
    },
    // Cycles 2-5 cover reading the operand & address depending on the addressing mode
    ReadingOperand {
        opcode: &'static Opcode,
        address_low_byte: Option<u8>,
        address_high_byte: Option<u8>,
        pointer: Option<u8>,
        indirect_address_low_byte: Option<u8>,
        indirect_address_high_byte: Option<u8>,
        checked_page_boundary: bool,
    },
    BranchCrossesPageBoundary {
        opcode: &'static Opcode,
        address: Option<u16>,
        operand: Option<u8>,
    },
    PushRegisterOnStack {
        value: u8,
    },
    PreIncrementStackPointer {
        operation: Operation,
    },
    PullRegisterFromStack {
        operation: Operation,
    },
    PullPCLFromStack {
        operation: Operation,
    },
    PullPCHFromStack {
        operation: Operation,
        pcl: u8,
    },
    IncrementProgramCounter,
    WritePCHToStack {
        address: u16,
    },
    WritePCLToStack {
        address: u16,
    },
    SetProgramCounter {
        address: u16,
    },
    WritingResult {
        address: u16,
        value: u8,
        dummy: bool,
    },
}

pub struct Cpu<'a> {
    state: CpuState,
    registers: Registers,
    mmu: &'a mut Mmu<'a>,
    cycles: u32,
}

impl<'a> Cpu<'a> {
    pub(crate) fn new(mmu: &'a mut Mmu<'a>) -> Self {
        let r: Registers = Default::default();

        Cpu {
            state: CpuState::FetchOpcode,
            registers: r,
            mmu,
            cycles: 0,
        }
    }

    fn nes_test_log(&self, opcode: &Opcode) -> String {
        let pc_1 = self.mmu.read_byte(self.registers.program_counter);
        let pc_2 = self.mmu.read_byte(self.registers.program_counter + 1);
        format!(
            "{:04X}  {:} A:{:02X} X:{:02X} Y:{:02X} P:{:02X} SP:{:02X} CPUC:{:}",
            self.registers.program_counter - 1,
            opcode.nes_test_log(pc_1, pc_2),
            self.registers.a,
            self.registers.x,
            self.registers.y,
            self.registers.status_register.bits() | 0b0010_0000,
            self.registers.stack_pointer,
            self.cycles
        )
    }

    fn push_to_stack(&mut self, value: u8) {
        self.mmu
            .write_byte(self.registers.stack_pointer as u16 | 0x0100, value);
        self.registers.stack_pointer = self.registers.stack_pointer.wrapping_sub(1);
    }

    fn pop_from_stack(&mut self) -> u8 {
        self.registers.stack_pointer = self.registers.stack_pointer.wrapping_add(1);
        self.mmu
            .read_byte(self.registers.stack_pointer as u16 | 0x0100)
    }

    fn read_and_inc_program_counter(&mut self) -> u8 {
        let value = self.mmu.read_byte(self.registers.program_counter);
        self.registers.program_counter += 1;

        value
    }

    fn adc(&mut self, operand: u8) {
        let result: u16 = match self
            .registers
            .status_register
            .contains(StatusFlags::CARRY_FLAG)
        {
            true => 1u16 + self.registers.a as u16 + operand as u16,
            false => self.registers.a as u16 + operand as u16,
        };
        self.registers.status_register.set(
            StatusFlags::OVERFLOW_FLAG,
            (self.registers.a as u16 ^ result) & (operand as u16 ^ result) & 0x80 > 0,
        );
        self.registers.a = (result & 0xFF) as u8;
        self.registers
            .status_register
            .set(StatusFlags::ZERO_FLAG, self.registers.a == 0);
        self.registers.status_register.set(
            StatusFlags::NEGATIVE_FLAG,
            self.registers.a & 0b1000_0000 != 0,
        );
        self.registers
            .status_register
            .set(StatusFlags::CARRY_FLAG, result > u8::MAX as u16);
    }

    fn compare(&mut self, operand: u8, register: u8) {
        let result = register.wrapping_sub(operand);
        self.registers
            .status_register
            .set(StatusFlags::CARRY_FLAG, register >= operand);
        self.set_negative_zero_flags(result);
    }

    fn decrement(&mut self, value: u8) -> u8 {
        let result = value.wrapping_sub(1);
        self.set_negative_zero_flags(result);

        result
    }

    fn increment(&mut self, value: u8) -> u8 {
        let result = value.wrapping_add(1);
        self.set_negative_zero_flags(result);

        result
    }

    fn set_negative_zero_flags(&mut self, operand: u8) {
        self.registers
            .status_register
            .set(StatusFlags::ZERO_FLAG, operand == 0);
        self.registers
            .status_register
            .set(StatusFlags::NEGATIVE_FLAG, operand & 0b1000_0000 != 0);
    }

    fn next_absolute_mode_state(
        &mut self,
        opcode: &'static Opcode,
        address_low_byte: Option<u8>,
        address_high_byte: Option<u8>,
    ) -> CpuState {
        match (address_low_byte, address_high_byte) {
            // Cycle 2 - Read low byte
            (None, _) => CpuState::ReadingOperand {
                opcode,
                address_low_byte: Some(self.read_and_inc_program_counter()),
                address_high_byte,
                pointer: None,
                indirect_address_low_byte: None,
                indirect_address_high_byte: None,
                checked_page_boundary: false,
            },
            // Cycle 3 - Read high byte
            (Some(low_byte), None) => {
                let high_byte = self.read_and_inc_program_counter();

                match opcode.operation.instruction_type() {
                    // Some instructions don't make use of the value at the absolute address, some do
                    InstructionType::Jump | InstructionType::Write => opcode.execute(
                        self,
                        None,
                        Some(low_byte as u16 | ((high_byte as u16) << 8)),
                    ),
                    _ => CpuState::ReadingOperand {
                        opcode,
                        address_low_byte,
                        address_high_byte: Some(high_byte),
                        pointer: None,
                        indirect_address_low_byte: None,
                        indirect_address_high_byte: None,
                        checked_page_boundary: false,
                    },
                }
            }
            // Cycle 4 - Read $HHLL from memory as operand
            (Some(low_byte), Some(high_byte)) => {
                let address = low_byte as u16 | ((high_byte as u16) << 8);
                opcode.execute(
                    self,
                    Some(self.mmu.read_byte(address)),
                    Some(address),
                )
            }
        }
    }

    fn next_absolute_indexed_mode_state(
        &mut self,
        opcode: &'static Opcode,
        address_low_byte: Option<u8>,
        address_high_byte: Option<u8>,
        checked_page_boundary: bool,
        index: u8,
    ) -> CpuState {
        match (address_low_byte, address_high_byte) {
            // Cycle 2 - Read low byte
            (None, None) => CpuState::ReadingOperand {
                opcode,
                address_low_byte: Some(self.read_and_inc_program_counter()),
                address_high_byte,
                pointer: None,
                indirect_address_low_byte: None,
                indirect_address_high_byte: None,
                checked_page_boundary: false,
            },
            // Cycle 3 - Read high byte
            (Some(_), None) => {
                CpuState::ReadingOperand {
                    opcode,
                    address_low_byte,
                    address_high_byte: Some(self.read_and_inc_program_counter()),
                    pointer: None,
                    indirect_address_low_byte: None,
                    indirect_address_high_byte: None,
                    checked_page_boundary: false,
                }
            }
            // Cycle 4 - Read $HHLL from memory as operand
            (Some(low_byte), Some(high_byte)) => {
                let unindexed_address = low_byte as u16 | ((high_byte as u16) << 8);
                let correct_address = unindexed_address.wrapping_add(index as u16);

                if checked_page_boundary {
                    opcode.execute(
                        self,
                        Some(self.mmu.read_byte(correct_address)),
                        Some(correct_address),
                    )
                } else {
                    // Dummy read, whether or not we end up using it (may read from a memory mapped port)
                    let first_read_address = low_byte.wrapping_add(index) as u16 | ((high_byte as u16) << 8);
                    let _ = self.mmu.read_byte(first_read_address);

                    match opcode.operation.instruction_type() {
                        InstructionType::Read => {
                            if correct_address == first_read_address {
                                opcode.execute(
                                    self,
                                    Some(self.mmu.read_byte(correct_address)),
                                    Some(correct_address),
                                )
                            } else {
                                CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte,
                                    address_high_byte,
                                    pointer: None,
                                    indirect_address_low_byte: None,
                                    indirect_address_high_byte: None,
                                    checked_page_boundary: true,
                                }
                            }
                        }
                        InstructionType::ReadModifyWrite => {
                            // Instructions which both read & write will always read twice
                            CpuState::ReadingOperand {
                                opcode,
                                address_low_byte,
                                address_high_byte,
                                pointer: None,
                                indirect_address_low_byte: None,
                                indirect_address_high_byte: None,
                                checked_page_boundary: true,
                            }
                        }
                        _ => {
                            opcode.execute(
                                self,
                                Some(self.mmu.read_byte(correct_address)),
                                Some(correct_address),
                            )
                        }
                    }
                }
            }
            (_, _) => panic!(), // Coding bug, can't read high byte first
        }
    }
}

impl<'a> Iterator for Cpu<'a> {
    type Item = ();

    fn next(&mut self) -> Option<()> {
        self.state = match self.state {
            CpuState::FetchOpcode => {
                let opcode = &OPCODE_TABLE[self.read_and_inc_program_counter() as usize];

                debug!("{}", self.nes_test_log(opcode));

                match opcode.address_mode {
                    AddressingMode::Accumulator => CpuState::ThrowawayRead {
                        opcode,
                        operand: Some(self.registers.a),
                    },
                    AddressingMode::Implied => CpuState::ThrowawayRead {
                        opcode,
                        operand: None,
                    },
                    _ => CpuState::ReadingOperand {
                        opcode,
                        address_low_byte: None,
                        address_high_byte: None,
                        pointer: None,
                        indirect_address_low_byte: None,
                        indirect_address_high_byte: None,
                        checked_page_boundary: false,
                    },
                }
            }
            CpuState::ReadingOperand {
                opcode,
                address_low_byte,
                address_high_byte,
                pointer,
                indirect_address_low_byte,
                indirect_address_high_byte,
                checked_page_boundary,
            } => {
                match opcode.address_mode {
                    AddressingMode::Absolute => self.next_absolute_mode_state(
                        opcode,
                        address_low_byte,
                        address_high_byte,
                    ),
                    AddressingMode::AbsoluteXIndexed => self.next_absolute_indexed_mode_state(
                        opcode,
                        address_low_byte,
                        address_high_byte,
                        checked_page_boundary,
                        self.registers.x,
                    ),
                    AddressingMode::AbsoluteYIndexed => self.next_absolute_indexed_mode_state(
                        opcode,
                        address_low_byte,
                        address_high_byte,
                        checked_page_boundary,
                        self.registers.y,
                    ),
                    AddressingMode::Immediate => {
                        let operand = Some(self.read_and_inc_program_counter());
                        opcode.execute(
                            self,
                            operand,
                            Some(self.registers.program_counter.wrapping_sub(1)),
                        )
                    }
                    AddressingMode::Indirect => {
                        match (
                            indirect_address_low_byte,
                            indirect_address_high_byte,
                            address_low_byte,
                        ) {
                            (None, _, _) => {
                                // Cycle 1 - Read the indirect address low byte
                                CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte: None,
                                    address_high_byte: None,
                                    pointer: None,
                                    indirect_address_low_byte: Some(
                                        self.read_and_inc_program_counter(),
                                    ),
                                    indirect_address_high_byte: None,
                                    checked_page_boundary: false,
                                }
                            }
                            (Some(_), None, _) => {
                                // Cycle 2 - Read the indirect address high byte
                                CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte: None,
                                    address_high_byte: None,
                                    pointer: None,
                                    indirect_address_low_byte,
                                    indirect_address_high_byte: Some(
                                        self.read_and_inc_program_counter(),
                                    ),
                                    checked_page_boundary: false,
                                }
                            }
                            (Some(indirect_low_byte), Some(indirect_high_byte), None) => {
                                let indirect_address =
                                    (indirect_low_byte as u16) | ((indirect_high_byte as u16) << 8);

                                // Cycle 3 - Read the address low byte from the indirect address
                                CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte: Some(self.mmu.read_byte(indirect_address)),
                                    address_high_byte: None,
                                    pointer: None,
                                    indirect_address_low_byte,
                                    indirect_address_high_byte,
                                    checked_page_boundary: false,
                                }
                            }
                            (Some(indirect_low_byte), Some(indirect_high_byte), Some(low_byte)) => {
                                // Cycle 4 - Read the address high byte from the indirect address and immediately set the PC as this is always a JMP instruction
                                // Note - this is deliberately "bugged", JMP (0x01FF) will jump to 0x01FF | 0x0100 << 8 NOT 0x01FF | 0x0200 << 8 as you might imagine (this is a known 6502 cpu bug)
                                let indirect_address = (indirect_low_byte.wrapping_add(1) as u16)
                                    | ((indirect_high_byte as u16) << 8);
                                let high_byte = self.mmu.read_byte(indirect_address);

                                opcode.execute(
                                    self,
                                    None,
                                    Some((low_byte as u16) | ((high_byte as u16) << 8)),
                                )
                            }
                        }
                    }
                    AddressingMode::IndirectXIndexed => {
                        match (
                            indirect_address_low_byte,
                            pointer,
                            address_low_byte,
                            address_high_byte,
                        ) {
                            (None, _, _, _) => {
                                // Cycle 1 - Read the low byte of the indirect address
                                CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte,
                                    address_high_byte,
                                    pointer: None,
                                    indirect_address_low_byte: Some(
                                        self.read_and_inc_program_counter(),
                                    ),
                                    indirect_address_high_byte,
                                    checked_page_boundary: false,
                                }
                            }
                            (Some(_), None, _, _) => {
                                // Cycle 2 - Construct the pointer to the actual address
                                CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte,
                                    address_high_byte,
                                    pointer: indirect_address_low_byte,
                                    indirect_address_low_byte,
                                    indirect_address_high_byte,
                                    checked_page_boundary: false,
                                }
                            }
                            (Some(indirect_low_byte), Some(_), None, _) => {
                                // Cycle 3 - Read the low byte of the actual address
                                let address =
                                    indirect_low_byte.wrapping_add(self.registers.x) as u16;

                                CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte: Some(self.mmu.read_byte(address)),
                                    address_high_byte,
                                    pointer,
                                    indirect_address_low_byte,
                                    indirect_address_high_byte,
                                    checked_page_boundary: false,
                                }
                            }
                            (Some(indirect_low_byte), Some(_), Some(address_low_byte), None) => {
                                // Cycle 4 - Read the high byte of the actual address
                                let indirect_address_high_byte = indirect_low_byte
                                    .wrapping_add(self.registers.x)
                                    .wrapping_add(1)
                                    as u16;
                                let address_high_byte = self.mmu.read_byte(indirect_address_high_byte);

                                match opcode.operation.instruction_type() {
                                    InstructionType::Write => {
                                        let address = (address_low_byte as u16) | ((address_high_byte as u16) << 8);
                                        opcode.execute(self, Some(self.mmu.read_byte(address)), Some(address))
                                    },
                                    _ => CpuState::ReadingOperand {
                                        opcode,
                                        address_low_byte: Some(address_low_byte),
                                        address_high_byte: Some(address_high_byte),
                                        pointer,
                                        indirect_address_low_byte,
                                        indirect_address_high_byte: Some(indirect_address_high_byte as u8),
                                        checked_page_boundary: false,
                                    }
                                }
                            }
                            (Some(_), Some(_), Some(low_byte), Some(high_byte)) => {
                                let address = (low_byte as u16) | ((high_byte as u16) << 8);

                                // Cycle 5 - Read the operand and execute operation
                                opcode.execute(
                                    self,
                                    Some(self.mmu.read_byte(address)),
                                    Some(address),
                                )
                            }
                        }
                    }
                    AddressingMode::IndirectYIndexed => {
                        match (
                            indirect_address_low_byte,
                            address_low_byte,
                            address_high_byte,
                        ) {
                            (None, _, _) => {
                                // Cycle 2 - Read the low byte of the indirect address
                                CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte,
                                    address_high_byte,
                                    pointer: None,
                                    indirect_address_low_byte: Some(
                                        self.read_and_inc_program_counter(),
                                    ),
                                    indirect_address_high_byte,
                                    checked_page_boundary: false,
                                }
                            }
                            (Some(indirect_low_byte), None, _) => {
                                // Cycle 3 - Read the low byte of the actual address
                                CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte: Some(
                                        self.mmu.read_byte(indirect_low_byte as u16),
                                    ),
                                    address_high_byte,
                                    pointer: None,
                                    indirect_address_low_byte,
                                    indirect_address_high_byte,
                                    checked_page_boundary: false,
                                }
                            }
                            (Some(indirect_low_byte), Some(address_low_byte), None) => {
                                // Cycle 4 - Read the high byte of the actual address
                                CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte: Some(address_low_byte),
                                    address_high_byte: Some(self.mmu.read_byte(indirect_low_byte.wrapping_add(1) as u16)),
                                    pointer: Some(indirect_low_byte),
                                    indirect_address_low_byte,
                                    indirect_address_high_byte,
                                    checked_page_boundary: false,
                                }
                            }
                            (Some(_), Some(low_byte), Some(high_byte)) => {
                                // Cycle 5(/6) - Read the operand and execute the operation checking for crossing page boundary
                                let unindexed_address = (low_byte as u16) | ((high_byte as u16) << 8);
                                let address = unindexed_address.wrapping_add(self.registers.y as u16);

                                if checked_page_boundary || (unindexed_address >> 4) == (address >> 4) {
                                    opcode.execute(self, Some(self.mmu.read_byte(address)), Some(address))
                                } else {
                                    CpuState::ReadingOperand {
                                        opcode,
                                        address_low_byte: Some(low_byte),
                                        address_high_byte: Some(high_byte),
                                        pointer: None,
                                        indirect_address_low_byte,
                                        indirect_address_high_byte,
                                        checked_page_boundary: true,
                                    }
                                }
                            }
                        }
                    }
                    AddressingMode::Relative => {
                        // Cycle 1 - Get the relative index and store it in the operand for use in the instruction (it'll be a signed 8 bit relative index)
                        let relative_operand = self.read_and_inc_program_counter();

                        let branch = match opcode.operation {
                            Operation::BCC => !self
                                .registers
                                .status_register
                                .contains(StatusFlags::CARRY_FLAG),
                            Operation::BCS => self
                                .registers
                                .status_register
                                .contains(StatusFlags::CARRY_FLAG),
                            Operation::BEQ => self
                                .registers
                                .status_register
                                .contains(StatusFlags::ZERO_FLAG),
                            Operation::BMI => self
                                .registers
                                .status_register
                                .contains(StatusFlags::NEGATIVE_FLAG),
                            Operation::BNE => !self
                                .registers
                                .status_register
                                .contains(StatusFlags::ZERO_FLAG),
                            Operation::BPL => !self
                                .registers
                                .status_register
                                .contains(StatusFlags::NEGATIVE_FLAG),
                            Operation::BVC => !self
                                .registers
                                .status_register
                                .contains(StatusFlags::OVERFLOW_FLAG),
                            Operation::BVS => self
                                .registers
                                .status_register
                                .contains(StatusFlags::OVERFLOW_FLAG),
                            _ => panic!(),
                        };

                        if !branch {
                            CpuState::FetchOpcode
                        } else {
                            let address = self
                                .registers
                                .program_counter
                                .wrapping_add((relative_operand as i8) as u16);

                            if (address >> 8) != (self.registers.program_counter >> 8) {
                                CpuState::BranchCrossesPageBoundary {
                                    opcode,
                                    operand: Some(relative_operand),
                                    address: Some(address),
                                }
                            } else {
                                opcode.execute(self, Some(relative_operand), Some(address))
                            }
                        }
                    }
                    AddressingMode::ZeroPage => match address_low_byte {
                        None => {
                            let operand = self.read_and_inc_program_counter();

                            match opcode.operation.instruction_type() {
                                InstructionType::Write => {
                                    let address = operand as u16;

                                    opcode.execute(
                                        self,
                                        Some(self.mmu.read_byte(address)),
                                        Some(address),
                                    )
                                }
                                _ => CpuState::ReadingOperand {
                                    opcode,
                                    address_low_byte: Some(operand),
                                    address_high_byte: None,
                                    pointer: None,
                                    indirect_address_low_byte: None,
                                    indirect_address_high_byte: None,
                                    checked_page_boundary: false,
                                },
                            }
                        }
                        Some(low_byte) => {
                            let address = low_byte as u16;

                            opcode.execute(self, Some(self.mmu.read_byte(address)), Some(address))
                        }
                    }
                    AddressingMode::ZeroPageXIndexed => match (address_low_byte, address_high_byte) {
                        (None, _) => {
                            // Cycle 2 - Read the zero page low byte
                            CpuState::ReadingOperand {
                                opcode,
                                address_low_byte: Some(self.read_and_inc_program_counter()),
                                address_high_byte: None,
                                pointer: None,
                                indirect_address_low_byte: None,
                                indirect_address_high_byte: None,
                                checked_page_boundary: false,
                            }
                        }
                        (Some(low_byte), None) => {
                            // Cycle 3 - Dummy read of the unindexed address
                            let _ = self.mmu.read_byte(low_byte as u16);

                            match opcode.operation.instruction_type() {
                                InstructionType::Write => {
                                    let address = low_byte.wrapping_add(self.registers.x) as u16;

                                    opcode.execute(self, Some(self.mmu.read_byte(address)), Some(address))
                                }
                                _ => {
                                    CpuState::ReadingOperand {
                                        opcode,
                                        address_low_byte,
                                        address_high_byte: Some(0x0),
                                        pointer: None,
                                        indirect_address_low_byte: None,
                                        indirect_address_high_byte: None,
                                        checked_page_boundary: false,
                                    }
                                }
                            }
                        }
                        (Some(low_byte), Some(_)) => {
                            // Cycle 4 - Read operand from the indexed zero page address
                            let address = low_byte.wrapping_add(self.registers.x) as u16;

                            opcode.execute(self, Some(self.mmu.read_byte(address)), Some(address))
                        }
                    }
                    AddressingMode::ZeroPageYIndexed => match (address_low_byte, address_high_byte) {
                        (None, _) => {
                            // Cycle 2 - Read the zero page low byte
                            CpuState::ReadingOperand {
                                opcode,
                                address_low_byte: Some(self.read_and_inc_program_counter()),
                                address_high_byte: None,
                                pointer: None,
                                indirect_address_low_byte: None,
                                indirect_address_high_byte: None,
                                checked_page_boundary: false,
                            }
                        }
                        (Some(low_byte), None) => {
                            // Cycle 3 - Dummy read of the unindexed address
                            let _ = self.mmu.read_byte(low_byte as u16);

                            match opcode.operation.instruction_type() {
                                InstructionType::Write => {
                                    let address = low_byte.wrapping_add(self.registers.y) as u16;

                                    opcode.execute(self, Some(self.mmu.read_byte(address)), Some(address))
                                }
                                _ => {
                                    CpuState::ReadingOperand {
                                        opcode,
                                        address_low_byte,
                                        address_high_byte: Some(0x0),
                                        pointer: None,
                                        indirect_address_low_byte: None,
                                        indirect_address_high_byte: None,
                                        checked_page_boundary: false,
                                    }
                                }
                            }
                        }
                        (Some(low_byte), Some(_)) => {
                            // Cycle 4 - Read operand from the indexed zero page address
                            let address = low_byte.wrapping_add(self.registers.y) as u16;

                            opcode.execute(self, Some(self.mmu.read_byte(address)), Some(address))
                        }
                    }
                    _ => panic!("Invalid, can't read operand for addressing mode {:?}", opcode.address_mode)
                }
            }
            CpuState::ThrowawayRead { opcode, operand } => opcode.execute(self, operand, None),
            CpuState::PushRegisterOnStack { value } => {
                self.push_to_stack(value);

                CpuState::FetchOpcode
            }
            CpuState::PreIncrementStackPointer { operation } => match operation {
                Operation::PLA | Operation::PLP | Operation::RTI => {
                    CpuState::PullRegisterFromStack { operation }
                }
                Operation::RTS => CpuState::PullPCLFromStack { operation },
                _ => panic!(
                    "Attempt to access stack from invalid instruction {:?}",
                    operation
                ),
            },
            CpuState::PullRegisterFromStack { operation } => match operation {
                Operation::PLA => {
                    self.registers.a = self.pop_from_stack();
                    self.set_negative_zero_flags(self.registers.a);
                    CpuState::FetchOpcode
                }
                Operation::PLP => {
                    self.registers.status_register =
                        StatusFlags::from_bits_truncate(self.pop_from_stack() & 0b1100_1111);

                    CpuState::FetchOpcode
                }
                Operation::RTI => {
                    self.registers.status_register =
                        StatusFlags::from_bits_truncate(self.pop_from_stack() & 0b1100_1111);

                    CpuState::PullPCLFromStack { operation }
                }
                _ => panic!(
                    "Attempt to access stack from invalid instruction {:?}",
                    operation
                ),
            },
            CpuState::PullPCLFromStack { operation } => CpuState::PullPCHFromStack {
                operation,
                pcl: self.pop_from_stack(),
            },
            CpuState::PullPCHFromStack { operation, pcl } => {
                let pch = self.pop_from_stack();
                self.registers.program_counter = ((pch as u16) << 8) | pcl as u16;

                match operation {
                    Operation::RTS => CpuState::IncrementProgramCounter,
                    Operation::RTI => CpuState::FetchOpcode,
                    _ => panic!(
                        "Attempt to access stack from invalid instruction {:?}",
                        operation
                    ),
                }
            }
            CpuState::IncrementProgramCounter => {
                self.registers.program_counter = self.registers.program_counter.wrapping_add(1);

                CpuState::FetchOpcode
            }
            CpuState::WritePCHToStack { address } => {
                self.push_to_stack((self.registers.program_counter.wrapping_sub(1) >> 8) as u8);

                CpuState::WritePCLToStack { address }
            }
            CpuState::WritePCLToStack { address } => {
                self.push_to_stack((self.registers.program_counter.wrapping_sub(1) & 0xFF) as u8);

                CpuState::SetProgramCounter { address }
            }
            CpuState::SetProgramCounter { address } => {
                self.registers.program_counter = address;

                CpuState::FetchOpcode
            }
            CpuState::BranchCrossesPageBoundary {
                opcode,
                operand,
                address,
            } => opcode.execute(self, operand, address),
            CpuState::WritingResult { value, address, dummy: true} => {
                CpuState::WritingResult { value, address, dummy: false }
            },
            CpuState::WritingResult { value, address, dummy: false} => {
                self.mmu.write_byte(address, value);

                CpuState::FetchOpcode
            }
        };

        self.cycles += 1;

        // Does the cpu ever halt? If no return None, otherwise this is just an
        // infinite sequence. Maybe bad opcode? Undefined behaviour of some sort?
        None
    }
}