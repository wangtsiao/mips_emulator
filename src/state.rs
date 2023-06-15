use std::io::{Read, Write};
use crate::memory::Memory;
use crate::page::{PAGE_ADDR_MASK, PAGE_SIZE};
use log::debug;
use std::cmp::min;

pub const FD_STDIN: u32 = 0;
pub const FD_STDOUT: u32 = 1;
pub const FD_STDERR: u32 = 2;
pub const FD_HINT_READ: u32 = 3;
pub const FD_HINT_WRITE: u32 = 4;
pub const FD_PREIMAGE_READ: u32 = 5;
pub const FD_PREIMAGE_WRITE: u32 = 6;
pub const MIPS_EBADF:u32  = 9;

trait PreimageOracle {
    fn hint(&self, v: &[u8]);
    fn get_preimage(&self, k: [u8; 32]) -> Vec<u8>;
}

struct State {
    memory: Box<Memory>,

    preimage_key: [u8; 32],
    preimage_offset: u32,

    /// the 32 general purpose registers of MIPS.
    registers: [u32; 32],
    /// the pc register stores the current execution instruction address.
    pc: u32,
    /// the next pc stores the next execution instruction address.
    next_pc: u32,
    /// the hi register stores the multiplier/divider result high(remainder) part.
    hi: u32,
    /// the low register stores the multiplier/divider result low(quotient) part.
    lo: u32,

    /// heap handles the mmap syscall.
    heap: u32,
    /// step tracks the total step has been executed.
    step: u64,

    exited: bool,
    exit_code: u8,

    // last_hint is optional metadata, and not part of the VM state itself.
    // It is used to remember the last pre-image hint,
    // so a VM can start from any state without fetching prior pre-images,
    // and instead just repeat the last hint on setup,
    // to make sure pre-image requests can be served.
    // The first 4 bytes are a uin32 length prefix.
    // Warning: the hint MAY NOT BE COMPLETE. I.e. this is buffered,
    // and should only be read when len(LastHint) > 4 && uint32(LastHint[:4]) >= len(LastHint[4:])
    last_hint: Vec<u8>,
}

pub struct InstrumentedState {
    /// state stores the state of the MIPS emulator
    state: State,

    /// writer for stdout
    stdout_writer: Box<dyn Write>,
    /// writer for stderr
    stderr_writer: Box<dyn Write>,

    /// track the memory address last time accessed.
    last_mem_access: u32,
    /// indicates whether enable memory proof.
    mem_proof_enabled: bool,
    /// merkle proof for memory, depth is 28.
    // todo: not sure the poseidon hash length, maybe not 32 bytes.
    mem_proof: [u8; 28*32],

    preimage_oracle: Box<dyn PreimageOracle>,

    last_preimage: Vec<u8>,
    last_preimage_key: [u8; 32],
    last_preimage_offset: u32,
}

impl InstrumentedState {
    fn track_memory_access(&mut self, addr: u32) {
        if self.mem_proof_enabled && self.last_mem_access != addr {
            panic!("unexpected different memory access at {:x?}, \
            already have access at {:x?} buffered", addr, self.last_mem_access);
        }
        self.last_mem_access = addr;
        self.mem_proof = self.state.memory.merkle_proof(addr);
    }

    // (data, data_len) = self.read_preimage(self.state.preimage_key, self.state.preimage_offset)
    pub fn read_preimage(&mut self, key: [u8; 32], offset: u32) -> ([u8; 32], u32) {
        if key != self.last_preimage_key {
            self.last_preimage_key = key;
            let data = self.preimage_oracle.get_preimage(key);
            // add the length prefix
            let mut preimage = Vec::new();
            preimage.extend(data.len().to_be_bytes());
            preimage.extend(data);
            self.last_preimage = preimage;
        }
        self.last_preimage_offset = offset;

        let mut data = [0; 32];
        let bytes_to_copy = &self.last_preimage[(offset as usize)..];
        let copy_size = bytes_to_copy.len().min(data.len());

        data[..copy_size].copy_from_slice(bytes_to_copy);
        return (data, copy_size as u32);
    }

    fn handle_syscall(&mut self) {
        let syscall_num = self.state.registers[2]; // v0
        let mut v0 = 0u32;
        let mut v1 = 0u32;

        let a0 = self.state.registers[4];
        let a1 = self.state.registers[5];
        let mut a2 = self.state.registers[6];

        match syscall_num {
            4090 => { // mmap
                // args: a0 = heap/hint, indicates mmap heap or hint. a1 = size
                let mut size = a1;
                if size&(PAGE_ADDR_MASK as u32) != 0 {
                    // adjust size to align with page size
                    size += PAGE_SIZE as u32 - (size & (PAGE_ADDR_MASK as u32));
                }
                if a0 == 0 {
                    v0 = self.state.heap;
                    self.state.heap += size;
                    debug!("mmap heap {:x?} size {:x?}", v0, size);
                } else {
                    v0 = a0;
                    debug!("mmap hint {:x?} size {:x?}", v0, size);
                }
            }
            4045 => { // brk
                v0 = 0x40000000;
            }
            4120 => { // clone
                v0 = 1;
            }
            4246 => { // exit group
                self.state.exited = true;
                self.state.exit_code = a0 as u8;
                return;
            }
            4003 => { // read
                // args: a0 = fd, a1 = addr, a2 = count
                // returns: v0 = read, v1 = err code
                match a0 {
                    FD_STDIN => {
                        // leave v0 and v1 zero: read nothing, no error
                    }
                    FD_PREIMAGE_READ => { // pre-image oracle
                        let addr = a1 & 0xFFffFFfc; // align memory
                        self.track_memory_access(addr);
                        let mem = self.state.memory.get_memory(addr);
                        let (data, mut data_len) =
                            self.read_preimage(self.state.preimage_key, self.state.preimage_offset);

                        let alignment = a1 & 3;
                        let space = 4 - alignment;
                        data_len = min(min(data_len, space), a2);

                        let mut out_mem = mem.to_be_bytes().clone();
                        out_mem[(alignment as usize)..].copy_from_slice(&data[..(data_len as usize)]);
                        self.state.memory.set_memory(addr, u32::from_be_bytes(out_mem));
                        self.state.preimage_offset += data_len;
                        v0 = data_len;
                    }
                    FD_HINT_READ => { // hint response
                        // don't actually read into memory,
                        // just say we read it all, we ignore the result anyway
                        v0 = a2;
                    }
                    _ => {
                        v0 = 0xFFffFFff;
                        v1 = MIPS_EBADF;
                    }
                }
            }
            4004 => { // write
                // args: a0 = fd, a1 = addr, a2 = count
                // returns: v0 = written, v1 = err code
                match a0 {
                    FD_STDOUT => {
                        self.state.memory.read_memory_range(a1, a2);
                        match std::io::copy(self.state.memory.as_mut(), self.stdout_writer.as_mut()) {
                            Err(e) => {
                                panic!("read range from memory failed {}", e);
                            }
                            Ok(_) => {}
                        }
                        v0 = a2;
                    }
                    FD_STDERR => {
                        self.state.memory.read_memory_range(a1, a2);
                        match std::io::copy(self.state.memory.as_mut(), self.stderr_writer.as_mut()) {
                            Err(e) => {
                                panic!("read range from memory failed {}", e);
                            }
                            Ok(_) => {}
                        }
                        v0 = a2;
                    }
                    FD_HINT_WRITE => {
                        self.state.memory.read_memory_range(a1, a2);
                        let mut hint_data = Vec::<u8>::new();
                        self.state.memory.read_to_end(&mut hint_data).unwrap();
                        self.state.last_hint.extend(&hint_data);
                        while self.state.last_hint.len() > 4 {
                            // process while there is enough data to check if there are any hints.
                            let mut hint_len_bytes = [0u8; 4];
                            hint_len_bytes.copy_from_slice(&self.state.last_hint[..4]);
                            let hint_len = u32::from_be_bytes(hint_len_bytes) as usize;
                            if hint_len >= self.state.last_hint[4..].len() {
                                let mut hint = Vec::<u8>::new();
                                self.state.last_hint[4..(4 + hint_len)].clone_into(&mut hint);
                                self.state.last_hint = self.state.last_hint.split_off(4+hint_len);
                                self.preimage_oracle.hint(hint.as_slice());
                            }
                        }
                    }
                    FD_PREIMAGE_WRITE => {
                        let addr = a1 & 0xFFffFFfc;
                        self.track_memory_access(addr);
                        let out_mem = self.state.memory.get_memory(addr);

                        let alignment = a1 & 3;
                        let space = 4 - alignment;
                        a2 = min(a2, space); // at most write to 4 bytes
                        let mut key = [0; 32];
                        key.copy_from_slice(&self.state.preimage_key[(a2 as usize)..]);
                        let dest_slice = &mut key[(32-a2 as usize)..];
                        dest_slice.copy_from_slice(&out_mem.to_be_bytes());
                        self.state.preimage_key = key;
                        self.state.preimage_offset = 0;
                        v0 = a2;
                    }
                    _ => {
                        v0 = 0xFFffFFff;
                        v1 = MIPS_EBADF;
                    }
                }
            }
            4055 => { // fcntl
                // args: a0 = fd, a1 = cmd
                if a1 == 3 { // F_GETFL: get file descriptor flags
                    match a0 {
                        FD_STDIN | FD_PREIMAGE_READ | FD_HINT_READ => {
                            v0 = 0 // O_RDONLY
                        }
                        FD_STDOUT | FD_STDERR | FD_PREIMAGE_WRITE | FD_HINT_WRITE => {
                            v0 = 1 // O_WRONLY
                        }
                        _ => {
                            v0 = 0xFFffFFff;
                            v1 = MIPS_EBADF;
                        }
                    }
                } else {
                    v0 = 0xFFffFFff;
                    v1 = MIPS_EBADF;
                }
            }
            _ => {}
        }

        self.state.registers[2] = v0;
        self.state.registers[7] = v1;

        self.state.pc = self.state.next_pc;
        self.state.next_pc = self.state.next_pc + 4;
    }

    pub fn handle_branch(&mut self, opcode: u32, insn: u32, rt_reg: u32, rs: u32) {
        if insn != 0 && insn != 1 {
            panic!("invalid insn when process branch on req imm, insn: {}", insn);
        }

        let should_branch = match opcode {
            4 | 5 => { // beq/bne
                let rt = self.state.registers[rt_reg as usize];
                (rs == rt && opcode == 4) || (rs != rt && opcode == 5)
            }
            6 => { // blez
                (rs as i32) <= 0
            }
            7 => { // bgtz
                (rs as i32) > 0
            }
            1 => { // reqimm
                let rtv = (insn >> 16) & 0x1F;
                if rtv == 0 { // bltz
                    (rs as i32) < 0
                } else { // 1 -> bgez
                    (rs as i32) >= 0
                }
            }
            _ => {
                panic!("invalid branch opcode {}", opcode);
            }
        };

        let prev_pc = self.state.pc;
        self.state.pc = self.state.next_pc; // execute the delay slot first
        if should_branch  {
            // then continue with the instruction the branch jumps to.
            self.state.next_pc = prev_pc + 4 + (sign_extension(insn&0xFFFF, 16) << 2);
        } else {
            self.state.next_pc = self.state.next_pc + 4;
        }
    }

    pub fn handle_jump(&mut self, link_reg: u32, dest: u32) {
        let prev_pc = self.state.pc;
        self.state.pc = self.state.next_pc;
        self.state.next_pc = dest;

        if link_reg != 0 {
            // set the link-register to the instr after the delay slot instruction.
            self.state.registers[link_reg as usize] = prev_pc + 8;
        }
    }

    pub fn handle_hilo(&mut self, fun: u32, rs: u32, rt: u32, store_reg: u32) {
        let mut val = 0u32;
        match fun {
            0x10 => { // mfhi
                val = self.state.hi;
            }
            0x11 => { // mthi
                self.state.hi = rs;
            }
            0x12 => { // mflo
                val = self.state.lo;
            }
            0x13 => { // mtlo
                self.state.lo = rs;
            }
            0x18 => { // mult
                let acc = (rs as i64 * rt as i64) as u64;
                self.state.hi = (acc >> 32) as u32;
                self.state.lo = acc as u32;
            }
            0x19 => { // mulu
                let acc = rs as u64 * rt as u64;
                self.state.hi = (acc >> 32) as u32;
                self.state.lo = acc as u32;
            }
            0x1a => { // div
                self.state.hi = ((rs as i32) % (rt as i32)) as u32;
                self.state.lo = ((rs as i32) / (rt as i32)) as u32;
            }
            0x1b => { // divu
                self.state.hi = rs % rt;
                self.state.lo = rs / rt;
            }
            n => {
                panic!("invalid fun when process hi lo, fun: {}", n);
            }
        }

        if store_reg != 0 {
            self.state.registers[store_reg as usize] = val;
        }

        self.state.pc = self.state.next_pc;
        self.state.next_pc = self.state.next_pc + 4;
    }

    pub fn handle_rd(&mut self, store_reg: u32, val: u32, conditional: bool) {
        if store_reg >=32 {
            panic!("invalid register");
        }
        if store_reg != 0 && conditional {
            self.state.registers[store_reg as usize] = val;
        }

        self.state.pc = self.state.next_pc;
        self.state.next_pc = self.state.next_pc + 4;
    }

    pub fn mips_step(&mut self) {
        if self.state.exited {
            return;
        }

        self.state.step += 1;

        // fetch instruction
        let insn = self.state.memory.get_memory(self.state.pc);
        let opcode = insn >> 26; // 6-bits

        // j-type j/jal
        if opcode == 2 || opcode == 3 {
            let link_reg = match opcode {
                3 => { 31 }
                _ => { 0 }
            };

            return self.handle_jump(link_reg, sign_extension(insn&0x03ffFFff, 26)<<2);
        }

        // fetch register
        let mut rt = 0u32;
        let rt_reg = (insn >> 16) & 0x1f;

        // R-type or I-type (stores rt)
        let mut rs = self.state.registers[((insn >> 21) & 0x1f) as usize];
        let mut rd_reg = rt_reg;
        if opcode == 0 || opcode == 0x1c {
            // R-type (stores rd)
            rt = self.state.registers[rt as usize];
            rd_reg = (insn >> 11) & 0x1f;
        } else if opcode < 0x20 {
            // rt is SignExtImm
            // don't sign extend for andi, ori, xori
            if opcode == 0xC || opcode == 0xD || opcode == 0xE {
                // ZeroExtImm
                rt = insn & 0xFFFF;
            } else {
                rt = sign_extension(insn&0xffFF, 16);
            }
        } else if opcode >= 0x28 || opcode == 0x22 || opcode == 0x26 {
            // store rt value with store
            rt = self.state.registers[rt_reg as usize];

            // store actual rt with lwl and lwr
            rd_reg = rt_reg;
        }

        if (opcode >= 4 && opcode < 8) || opcode == 1 {
            return self.handle_branch(opcode, insn, rt_reg, rs);
        }

        let mut store_addr: u32 = 0xffFFffFF;
        // memory fetch (all I-type)
        // we do the load for stores also
        let mut mem: u32 = 0;
        if opcode >= 0x20 {
            // M[R[rs]+SignExtImm]
            rs += sign_extension(insn&0xffFF, 16);
            let addr = rs & 0xFFffFFfc;
            self.track_memory_access(addr);
            mem = self.state.memory.get_memory(addr);
            if opcode >= 0x28 && opcode != 0x30 {
                // store
                store_addr = addr;
                // store opcodes don't write back to a register
                rd_reg = 0;
            }
        }

        // ALU
        let val = self.execute(insn, rs, rt, mem);

        let fun = insn & 0x3f; // 6-bits
        if opcode == 0 && fun >= 8 && fun < 0x1c {
            if fun == 8 || fun ==9 {
                let link_reg = match fun {
                    9=> {rd_reg},
                    _=> {0}
                };
                return self.handle_jump(link_reg, rs);
            }

            if fun == 0xa {
                return self.handle_rd(rd_reg, rs, rt == 0);
            }
            if fun == 0xb {
                return self.handle_rd(rd_reg, rs, rt != 0);
            }

            // syscall (can read/write)
            if fun == 0xc {
                return self.handle_syscall();
            }

            // lo and hi registers
            // can write back
            if fun >= 0x10 && fun < 0x1c {
                return self.handle_hilo(fun, rs, rt, rd_reg);
            }
        }

        // stupid sc, write a 1 to rt
        if opcode == 0x38 && rt_reg != 0 {
            self.state.registers[rt_reg as usize] = 1;
        }

        // write memory
        if store_addr != 0xffFFffFF {
            self.track_memory_access(store_addr);
            self.state.memory.set_memory(store_addr, val);
        }

        // write back the value to the destination register
        return self.handle_rd(rd_reg, val, true);
    }

    fn execute(&mut self, insn: u32, mut rs: u32, rt: u32, mem: u32) -> u32 {
        // implement alu
        let mut opcode = insn >> 26;
        let mut fun = insn & 0x3F;

        if opcode < 0x20 {
            // transform ArithLogI
            if opcode >= 8 && opcode < 0xf {
                match opcode {
                    8 => {
                        fun = 0x20; // addi
                    }
                    9=> {
                        fun = 0x21; // addiu
                    }
                    0xa => {
                        fun = 0x2a; // slti
                    }
                    0xb => {
                        fun = 0x2b; // sltiu
                    }
                    0xc => {
                        fun = 0x24; // andi
                    }
                    0xd => {
                        fun = 0x25; // ori
                    }
                    0xe => {
                        fun = 0x26; // xori
                    }
                    _ => {}
                }
                opcode = 0;
            }

            // 0 is opcode SPECIAL
            if opcode == 0 {
                let shamt = (insn >> 6) & 0x1f;
                if fun < 0x20 {
                    if fun >= 0x08 {
                        return rs; // jr/jalr/div + others
                    } else if fun == 0x00 {
                        return rt << shamt; // sll
                    } else if fun == 0x02 {
                        return rt >> shamt; // srl
                    } else if fun == 0x03 {
                        return sign_extension(rt >> shamt, 32-shamt); // sra
                    } else if fun == 0x04 {
                        return rt << (rs & 0x1f); // sllv
                    } else if fun == 0x06 {
                        return rt >> (rs & 0x1f); // srlv
                    } else if fun == 0x07 {
                        return sign_extension(rt>>rs, 32-rs); // srav
                    }
                }

                // 0x10 - 0x13 = mfhi, mthi, mflo, mtlo
                // R-type (ArithLog)
                match fun {
                    0x20 | 0x21 => {
                        return rs + rt; // add or addu
                    }
                    0x22 | 0x23 => {
                        return rs - rt; // sub or subu
                    }
                    0x24 => {
                        return rs & rt; // and
                    }
                    0x25 => {
                        return rs ^ rt; // xor
                    }
                    0x27 => {
                        return !(rs | rt); // nor
                    }
                    0x2a => {
                        return if (rs as i32) < (rt as i32) {
                            1 // slt
                        } else {
                            0
                        }
                    }
                    0x2b => {
                        return if rs < rt {
                            1 // sltu
                        } else {
                            0
                        }
                    }
                    _ => {}
                }
            } else if opcode == 0xf {
                return rt << 16; // lui
            } else if opcode == 0x1c { // SPECIAL2
                if fun == 2 { // mul
                    return ((rs as i32) * (rt as i32)) as u32;
                }
                if fun == 0x20 || fun == 0x21 { // clo
                    if fun == 0x20 {
                        rs = !rs;
                    }
                    let mut i = 0;
                    while rs & 0x80000000 != 0 {
                        rs <<= 1;
                        i += 1;
                    }
                    return i;
                }
            }
        } else if opcode < 0x28 {
            match opcode {
                0x20 => { // lb
                    return sign_extension((mem>>(24-(rs&3)*8))&0xff, 8);
                }
                0x21 => { // lh
                    return sign_extension((mem>>(16-(rs&2)*8))&0xffff, 16);
                }
                0x22 => { // lwl
                    let val = mem << ((rs & 3) * 8);
                    let mask = 0xffFFffFFu32 << ((rs & 3) * 8);
                    return (rt & (!mask)) | val;
                }
                0x23 => { // lw
                    return mem;
                }
                0x24 => { // lbu
                    return (mem >> (24 - (rs & 3)*8)) & 0xff;
                }
                0x25 => { // lhu
                    return (mem >> (16 - (rs & 2)*8)) & 0xffff;
                }
                0x26 => { // lwr
                    let val = mem >> (24 - (rs&3)*8);
                    let mask = 0xffFFffFFu32 >> (24 - (rs&3)*8);
                    return (rt & (!mask)) | val;
                }
                _ => {}
            }
        } else if opcode == 0x28 { // sb
            let val = (rt & 0xff) << (24 - (rs&3)*8);
            let mask = 0xffFFffFFu32 ^ (0xff<<(24-(rs&3)*8));
            return (mem & mask) | val;
        } else if opcode == 0x29 { // sh
            let val = (rt & 0xffff) << (16 - (rs&2)*8);
            let mask = 0xffFFffFFu32 ^ (0xffff<<(16-(rs&2)*8));
            return (mem & mask) | val;
        } else if opcode == 0x2a { // swl
            let val = rt >> ((rs & 3) * 8);
            let mask = 0xffFFffFFu32 >> ((rs & 3) * 8);
            return (mem & (!mask)) | val;
        } else if opcode == 0x2b { // sw
            return rt;
        } else if opcode == 0x2e { // swr
            let val = rt << (24 - (rs & 3) *8 );
            let mask = 0xffFFffFFu32 << (24 - (rs & 3) *8 );
            return (mem & (!mask)) | val;
        } else if opcode == 0x30 { // ll
            return mem;
        } else if opcode == 0x38 { // lc
            return rt;
        }

        panic!("invalid instruction, opcode: {}", opcode);
    }
}

/// se extends the number to 32 bit with sign.
fn sign_extension(dat: u32, idx: u32) -> u32 {
    let is_signed = (dat >> (idx-1)) != 0;
    let signed = ((1u32 << (32-idx)) - 1) << idx;
    let mask = (1u32 << idx) - 1;
    if is_signed {
        dat & mask | signed
    } else {
        dat & mask
    }
}
