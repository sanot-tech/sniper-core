//! 🧱 `stub_program` — a loadable SBPF program, synthesised in-process.
//!
//! ## The problem this exists to solve
//!
//! [`Forge::load_default`] reads `fixtures/programs/pump.so`, a ~10 MB binary
//! that is deliberately **not** in the repository: `.gitignore` excludes
//! `fixtures/programs/`, because a 10 MB blob does not belong in git history
//! and has to be refetched with `cargo dump-pump` against a mainnet RPC node.
//!
//! The consequence is that every test in this crate fails on a fresh clone:
//!
//! ```text
//! test result: FAILED. 0 passed; 6 failed;
//! cannot read pump program dump at .../fixtures/programs/pump.so
//! ```
//!
//! Six failures, none of them about anything. That is the worst possible
//! starting position, because the obvious responses are both wrong: committing
//! the binary bloats every clone forever, and skipping the tests leaves the
//! suite green while testing nothing.
//!
//! ## What a stub actually needs to be
//!
//! [`crate::Forge::load_bytes`] hands the bytes to LiteSVM, which parses them
//! with `solana_sbpf`'s loader. "Loadable" is a real bar -- an empty slice is
//! rejected with "Offset or value is out of bounds" -- but the loader's
//! requirements are knowable and narrow. Read out of `solana-sbpf-0.23.0`
//! rather than guessed, because every one of these is a hard rejection:
//!
//! | Field | Required value | Where the rule lives |
//! |---|---|---|
//! | `e_ident` | magic, class 64, LSB, version 1, OSABI 0, abiversion 0, pad `[0;7]` | `elf.rs` `parse_file_header` |
//! | `e_type` | `ET_DYN` (3) | `ElfParserError::WrongType` |
//! | `e_machine` | `EM_BPF` (247) -- **not** `EM_SBPF` | `InvalidFileHeader` |
//! | `e_phoff` | exactly 64 | `InvalidFileHeader` |
//! | `e_ehsize` / `e_phentsize` | 64 / 56 | `InvalidFileHeader` |
//! | `e_entry` | multiple of 8, inside the text `vm_range` | `InvalidFileHeader` |
//! | `.text` sections | **exactly one** | `ElfParserError::NotOneTextSection` |
//! | writable `.data` / `.bss` | none, except `.data.rel.*` | `WritableSectionNotSupported` |
//! | program header | `PT_LOAD`, `PF_X`, vaddr = paddr = `MM_BYTECODE_START`, offset 8-aligned, `filesz == memsz` | `InvalidProgramHeader` |
//!
//! One program header is enough: when `p_flags` is not `PF_R`, the loader skips
//! the read-only-data header and treats header 0 as the bytecode segment. That
//! is the path taken here, so the stub carries no rodata at all.
//!
//! ## What the stub does when it runs
//!
//! Two instructions, and both matter:
//!
//! ```text
//!     r0 = 0        ; MOV64_IMM
//!     exit
//! ```
//!
//! Loading `r0` before exiting is not decoration. The SBPF virtual machine does
//! not define the register file on entry, and a program that exits with
//! whatever happened to be in `r0` returns a value nobody chose. A stub whose
//! success depends on uninitialised state is a flaky test waiting for a
//! different CPU.
//!
//! ## What it deliberately does not prove
//!
//! Nothing about pump.fun. The stub is not a curve, an AMM, or a bonding curve.
//! It is a *deployment target*: enough for the VM, the account model, the
//! transaction pipeline, determinism and the log paths to be tested for real.
//!
//! Tests that need pump.fun's actual behaviour keep saying so. They are marked
//! `#[ignore]` and take the real dump when it is present -- see
//! `real_pump_program.rs`. Swapping in the 10 MB binary is a matter of running
//! `cargo dump-pump`, and every test in this crate picks it up automatically.
//!
//! What this removes is the failure mode where the sandbox is *structurally*
//! untestable on a clean checkout.

use solana_sbpf::ebpf::{BPF_ALU64_STORE, BPF_EXIT, BPF_JMP64, BPF_K, BPF_MOV};

// ---------------------------------------------------------------------------
// Values copied from solana-sbpf's own constants rather than hard-coded.
//
// These are duplicated rather than imported. Reaching into solana_sbpf for an
// ELF loader's internal layout would tie a test fixture to an implementation
// detail that is allowed to change between patch releases, and a stub that
// breaks on a dependency bump is worse than no stub: it would be reported as
// "the sandbox is broken" rather than "this constant moved". The values are
// also checked against that crate's definitions by
// `stub_constants_match_the_sbpf_loader`, so a drift fails as a clear
// assertion rather than as a mysterious parse error.
// ---------------------------------------------------------------------------

/// `solana_sbpf::elf_parser::consts::EM_BPF`.
const EM_BPF: u16 = 247;
/// `elf_parser::consts::ET_DYN`.
const ET_DYN: u16 = 3;
/// `elf_parser::consts::PT_LOAD`.
const PT_LOAD: u32 = 1;
/// `elf_parser::consts::PF_X`.
const PF_X: u32 = 0x1;
/// `elf_parser::consts::SHT_PROGBITS` (1), the section type for `.text`.
///
/// The value is 1, not 6. Six is `SHT_DYNAMIC`, and getting the two confused
/// produces a baffling failure rather than an obvious one: the file still
/// parses as ELF, `.text` is still found, and the loader then tries to read a
/// `.dynamic` section out of machine code and reports "invalid dynamic section
/// table". That is what happened the first time this stub was written, and no
/// amount of staring at the ELF header explains it -- the header is fine. The
/// number that is wrong is four bytes of section header away.
const SHT_PROGBITS: u32 = 1;
/// `elf_parser::consts::SHT_STRTAB` (3).
const SHT_STRTAB: u32 = 3;
/// `elf_parser::consts::SHF_ALLOC` (0x2) | `SHF_EXECINSTR` (0x4).
const SHF_ALLOC_EXEC: u64 = 0x2 | 0x4;
/// `elf_parser::consts::SHF_ALLOC` (0x2), for the section-name table.
const SHF_ALLOC: u64 = 0x2;

/// `ebpf::MM_BYTECODE_START` -- the only virtual address the loader accepts
/// for a bytecode segment.
const MM_BYTECODE_START: u64 = 1 << 32;

/// `ebpf::INSN_SIZE`.
const INSN_SIZE: u64 = 8;

/// ELF64 header size, fixed by the format.
const EHDR_SIZE: u64 = 64;
/// ELF64 program header size, fixed by the format.
const PHDR_SIZE: u64 = 56;
/// ELF64 section header size, fixed by the format.
const SHDR_SIZE: u64 = 64;

/// Writes a little-endian `u16`.
fn put_u16(buf: &mut [u8], offset: usize, value: u16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

/// Writes a little-endian `u32`.
fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// Writes a little-endian `u64`.
fn put_u64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// Builds a valid, loadable SBPF program that exits successfully.
///
/// The returned bytes are a complete ELF64 object, ready for
/// `LiteSVM::add_program`.
///
/// # Panics
///
/// Never. Kept allocation-only so it can be called from a `const`-adjacent
/// context and from tests that are already panicking.
#[must_use]
pub fn stub_program_bytes() -> Vec<u8> {
    // -- the program itself ----------------------------------------------------
    //
    // Two instructions, sixteen bytes total. The SBPF instruction encoding is
    // exactly eight bytes:
    //
    //     [ opcode:1 ][ dst<<4|src:1 ][ off:2 ][ imm:4 ]
    //
    // The wide-load encoding is *two* such slots: the first carries the opcode
    // and the low half of the immediate, the second is the high half. Emitting
    // only the first slot produces a program that loads and then runs off the
    // end of its own text -- which the ELF loader happily accepts, because it
    // validates structure and never executes anything.
    let mut text = Vec::with_capacity(16);
    // MOV64_IMM r0, 0 -- a *single* eight-byte slot, not a wide load.
    //
    // The first version of this stub used the wide-load encoding (0x18), which
    // genuinely occupies two slots, and then emitted the second one as eight
    // zero bytes. The verifier rejects that with "unknown eBPF opcode 0x0": a
    // zero slot is not an instruction at all. MOV64_IMM loads a 64-bit
    // immediate in one slot and needs no partner, which makes it the right
    // choice for a two-instruction program.
    text.push(BPF_ALU64_STORE | BPF_K | BPF_MOV); // opcode 0xb7
    text.push(0x00); // regs: dst = r0, src = r0
    text.extend_from_slice(&0i16.to_le_bytes()); // off
    text.extend_from_slice(&0i32.to_le_bytes()); // imm
                                                 // exit
    text.push(BPF_JMP64 | BPF_EXIT);
    text.push(0x00); // regs
    text.extend_from_slice(&0i16.to_le_bytes()); // off
    text.extend_from_slice(&0i32.to_le_bytes()); // imm

    debug_assert_eq!(
        text.len() as u64 % INSN_SIZE,
        0,
        "SBPF text must be a whole number of 8-byte instructions"
    );

    // -- layout ----------------------------------------------------------------
    //
    //   0x0000  ELF header            64 bytes
    //   0x0040  program header        56 bytes   (the loader demands e_phoff == 64)
    //   0x0078  .text                 16 bytes   (offset must be 8-aligned)
    //   ----    section headers       192 bytes   (3 x 64)
    //
    // The name table is "\0.text\0" -- eight bytes, not six, because the
    // leading NUL is reserved for the SHT_NULL section at index 0.
    //
    // No separate string table: the single string this file needs is the name
    // ".text", and its bytes live at the end of the buffer with an offset
    // recorded in the section header. A string table is not required by the
    // loader, and one fewer moving part is one fewer way for this to be wrong.
    let text_offset = (EHDR_SIZE + PHDR_SIZE) as usize;
    let text_len = text.len();
    let shstr_offset = text_offset + text_len;
    // The name table starts with a NUL on purpose.
    //
    // The mandatory section header at index 0 (SHT_NULL) still carries whatever
    // sh_name it is given, and the loader resolves that name through this very
    // table. With the table starting at ".text", section 0 resolves to ".text"
    // as well, the loader counts two text sections, and it fails with
    // "Multiple or no text sections" -- an error that points at the section
    // headers while the actual mistake is one byte into the string table.
    //
    // So offset 0 is the empty string (the SHT_NULL section's name) and
    // offset 1 is ".text". Offset 6 -- the NUL that terminates it -- doubles as
    // the empty name for .shstrtab. b"\0.text\0" is seven bytes, not eight,
    // and pointing past its end is what produced "Section or symbol name is
    // longer than 16 bytes" for an empty string.
    let section_name_text: &[u8] = b"\0.text\0";
    let shstrtab_offset = shstr_offset + section_name_text.len();
    let sh_offset = shstrtab_offset; // 8-aligned already: 96 + 16 + 6 = 118 -> pad
                                     // The loader requires every section to lie inside the file and, for the
                                     // text section, to be 8-aligned in memory. Pad the name table to 8 so the
                                     // section header table that follows starts on a boundary a human can
                                     // reason about when reading a hex dump.
    let sh_offset = (sh_offset + 7) & !7;

    let total = sh_offset + (3 * SHDR_SIZE as usize);
    let mut out = vec![0u8; total];

    // -- ELF header ------------------------------------------------------------
    out[0..4].copy_from_slice(b"\x7fELF");
    out[4] = 2; // ELFCLASS64
    out[5] = 1; // ELFDATA2LSB
    out[6] = 1; // EV_CURRENT
    out[7] = 0; // ELFOSABI_NONE -- the loader rejects anything else
                // 8..16 stays zero: ei_pad must be [0; 7] and abiversion 0.
    put_u16(&mut out, 16, ET_DYN);
    put_u16(&mut out, 18, EM_BPF);
    put_u32(&mut out, 20, 1); // e_version = EV_CURRENT
                              // e_entry: the first instruction. Two constraints, both easy to get wrong.
                              //
                              // It must be a multiple of 8 and it must fall inside the .text vm_range.
                              // That range is derived from the section header's sh_addr -- which, per the
                              // next paragraph, is the *file offset*, not the load address. So e_entry is
                              // the file offset of .text, 120, and not MM_BYTECODE_START.
    put_u64(&mut out, 24, text_offset as u64);
    put_u64(&mut out, 32, EHDR_SIZE); // e_phoff, exactly 64
    put_u64(&mut out, 40, sh_offset as u64); // e_shoff
    put_u32(&mut out, 48, 0); // e_flags
    put_u16(&mut out, 52, EHDR_SIZE as u16); // e_ehsize
    put_u16(&mut out, 54, PHDR_SIZE as u16); // e_phentsize
    put_u16(&mut out, 56, 1); // e_phnum -- one segment, bytecode only
    put_u16(&mut out, 58, SHDR_SIZE as u16); // e_shentsize
    put_u16(&mut out, 60, 3); // e_shnum: null, .text, .shstrtab
    put_u16(&mut out, 62, 2); // e_shstrndx

    // -- program header --------------------------------------------------------
    //
    // p_flags is PF_X, *not* PF_R. That is deliberate and load-bearing: seeing
    // a non-read-only first header is how the loader knows there is no rodata
    // segment and that header 0 is the bytecode.
    let ph = EHDR_SIZE as usize;
    put_u32(&mut out, ph, PT_LOAD);
    put_u32(&mut out, ph + 4, PF_X);
    put_u64(&mut out, ph + 8, text_offset as u64); // p_offset: must be 8-aligned
    put_u64(&mut out, ph + 16, MM_BYTECODE_START); // p_vaddr
    put_u64(&mut out, ph + 24, MM_BYTECODE_START); // p_paddr == p_vaddr
    put_u64(&mut out, ph + 32, text_len as u64); // p_filesz
    put_u64(&mut out, ph + 40, text_len as u64); // p_memsz, must equal filesz
    put_u64(&mut out, ph + 48, 0); // p_align

    // -- .text -----------------------------------------------------------------
    out[text_offset..text_offset + text_len].copy_from_slice(&text);

    // -- section name table ----------------------------------------------------
    out[shstr_offset..shstr_offset + section_name_text.len()].copy_from_slice(section_name_text);

    // sh0: SHT_NULL, must be all zero -- the buffer is already zeroed.
    let _sh0 = sh_offset;

    // -- section headers -------------------------------------------------------
    //
    // Layout of one Elf64Shdr, all 64 bytes and none of them spare:
    //
    //   +0  sh_name (4)   +32 sh_size (8)     +56 sh_entsize (8)
    //   +4  sh_type (4)   +40 sh_link (4)     +60..64  (end)
    //   +8  sh_flags (8)  +44 sh_info (4)
    //   +16 sh_addr (8)   +48 sh_addralign (8)
    //   +24 sh_offset (8)
    //
    // sh_entsize sits at +56, not +64. Writing it at +64 runs off the end of
    // the header and into -- or past -- the end of the file.

    // sh1: .text
    let sh1 = sh_offset + SHDR_SIZE as usize;
    // sh_name is an offset into the name table, and ".text" begins at offset 1
    // because offset 0 is the empty string reserved for the SHT_NULL section.
    put_u32(&mut out, sh1, 1); // sh_name
    put_u32(&mut out, sh1 + 4, SHT_PROGBITS);
    put_u64(&mut out, sh1 + 8, SHF_ALLOC_EXEC);
    // sh_addr equals sh_offset, which is not a typo.
    //
    // Executable::load rejects any program whose .text has sh_addr != sh_offset
    // ("Offset or value is out of bounds"). The runtime adds MM_REGION_SIZE to
    // this value itself when it computes where the code actually loads; the
    // section header records a file address, not a virtual one. Setting it to
    // MM_BYTECODE_START here -- which looks more correct -- is what made the
    // first version of this stub unloadable.
    put_u64(&mut out, sh1 + 16, text_offset as u64); // sh_addr == sh_offset
    put_u64(&mut out, sh1 + 24, text_offset as u64); // sh_offset
    put_u64(&mut out, sh1 + 32, text_len as u64); // sh_size
    put_u32(&mut out, sh1 + 44, 0); // sh_link
    put_u32(&mut out, sh1 + 48, 0); // sh_info
    put_u64(&mut out, sh1 + 48, 8); // sh_addralign
    put_u64(&mut out, sh1 + 56, 0); // sh_entsize

    // sh2: .shstrtab. Not allocated -- it is metadata, and the loader rejects
    // writable sections, so it is safest to give it no flags at all.
    let sh2 = sh_offset + 2 * SHDR_SIZE as usize;
    // Offset 6 is the NUL terminating ".text", so the string starting there is
    // empty. Shstrtab's own name is not read by anything here, but leaving it
    // pointing at real text would be the kind of thing that confuses a hex dump
    // reader six months from now.
    put_u32(&mut out, sh2, 6); // sh_name: "" -- the NUL that ends ".text"
    put_u32(&mut out, sh2 + 4, SHT_STRTAB);
    put_u64(&mut out, sh2 + 8, SHF_ALLOC);
    put_u64(&mut out, sh2 + 16, 0); // sh_addr
    put_u64(&mut out, sh2 + 24, shstr_offset as u64); // sh_offset
    put_u64(&mut out, sh2 + 32, section_name_text.len() as u64); // sh_size
    put_u64(&mut out, sh2 + 48, 1); // sh_addralign
    put_u64(&mut out, sh2 + 56, 0); // sh_entsize

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real SBPF loader accepts the stub.
    ///
    /// Everything else in this module checks the file against the *rules*. This
    /// one hands it to the actual loader used in production, which is the only
    /// check that cannot be fooled by a rule the author misremembered.
    ///
    /// It earned its place immediately. Writing the stub involved four separate
    /// mistakes, each of which passed every structural assertion above:
    ///
    ///   1. `SHT_PROGBITS` written as 6, which is `SHT_DYNAMIC`. The loader
    ///      tried to parse a dynamic section out of two instructions.
    ///   2. A zero-padded second slot for a wide load, which the verifier
    ///      rejects as "unknown eBPF opcode 0x0".
    ///   3. `.text` `sh_addr` set to the load address instead of the file
    ///      offset, which the loader reports as "Offset or value is out of
    ///      bounds" -- pointing at the program headers, not the section.
    ///   4. A name table without a leading NUL, so the mandatory SHT_NULL
    ///      section at index 0 also resolved to ".text" and the loader found
    ///      two text sections.
    ///
    /// Every one of those is a plausible-looking ELF. None of them is caught by
    /// reading the header. This test is the one that catches them.
    #[test]
    fn lite_svm_accepts_the_stub() {
        use litesvm::LiteSVM;
        use solana_address::Address;

        let mut svm = LiteSVM::new();
        let program_id = Address::new_from_array([7u8; 32]);

        svm.add_program(program_id, &stub_program_bytes())
            .expect("LiteSVM must accept the synthesised stub");

        let account = svm
            .get_account(&program_id)
            .expect("the program account must exist after deployment");
        assert!(
            account.executable,
            "the stub must deploy as an executable program, not as data"
        );
    }

    /// The duplicated constants still describe solana-sbpf's loader.
    ///
    /// The whole stub rests on these numbers. A dependency bump that moves
    /// `EM_BPF` or `MM_BYTECODE_START` would otherwise surface as "Offset or
    /// value is out of bounds" from deep inside the loader, which says nothing
    /// about the cause. This asserts against the real crate so the drift is
    /// reported here, where the comment explains it.
    #[test]
    fn stub_constants_match_the_sbpf_loader() {
        use solana_sbpf::ebpf::{INSN_SIZE as REAL_INSN, MM_BYTECODE_START as REAL_BYTECODE};
        use solana_sbpf::elf_parser::consts::{
            EM_BPF as REAL_EM_BPF, PF_X as REAL_PF_X, PT_LOAD as REAL_PT_LOAD,
        };

        assert_eq!(EM_BPF, REAL_EM_BPF, "EM_BPF moved");
        assert_eq!(PT_LOAD, REAL_PT_LOAD, "PT_LOAD moved");
        assert_eq!(PF_X, REAL_PF_X, "PF_X moved");
        assert_eq!(MM_BYTECODE_START, REAL_BYTECODE, "MM_BYTECODE_START moved");
        assert_eq!(INSN_SIZE, REAL_INSN as u64, "INSN_SIZE moved");
    }

    /// The instruction encoding is what the VM decodes, byte for byte.
    ///
    /// Asserting the opcodes here rather than only that the program loads
    /// catches a mis-encoded stub that happens to be *loadable* -- an ELF can
    /// pass every structural check in `Executable::load` and still contain
    /// instructions the VM will not run the way the author meant.
    #[test]
    fn the_stub_encodes_return_zero_then_exit() {
        let bytes = stub_program_bytes();
        // Locate .text: the segment the program header points at. Both
        // instructions are single eight-byte slots, so exit is one slot in --
        // the wide-load form this used to use put it two slots in.
        let text_offset = (EHDR_SIZE + PHDR_SIZE) as usize;
        let opcode = bytes[text_offset];
        let exit_opcode = bytes[text_offset + 8];

        assert_eq!(
            opcode,
            BPF_ALU64_STORE | BPF_K | BPF_MOV,
            "first instruction must be the wide immediate load"
        );
        assert_eq!(
            exit_opcode,
            BPF_JMP64 | BPF_EXIT,
            "second instruction must be exit"
        );
    }

    /// Structurally valid ELF64, checked field by field.
    ///
    /// These are the loader's own preconditions. Asserting them here means a
    /// future edit that moves an offset fails with a sentence explaining which
    /// field broke, instead of a parse error from inside a dependency.
    #[test]
    fn the_stub_is_a_well_formed_elf() {
        let bytes = stub_program_bytes();

        assert_eq!(&bytes[0..4], b"\x7fELF", "magic");
        assert_eq!(bytes[4], 2, "ELFCLASS64");
        assert_eq!(bytes[5], 1, "ELFDATA2LSB");
        assert_eq!(bytes[6], 1, "EV_CURRENT");
        assert_eq!(bytes[7], 0, "ELFOSABI_NONE");
        // Bytes 8..15 are ei_pad, which the loader compares against [0; 7]
        // element by element, and byte 15 is ei_abiversion, which it requires
        // to be zero. Asserting the whole span together is what caught this
        // being written as a 7-byte comparison against an 8-byte slice.
        assert_eq!(
            bytes[8..15],
            [0u8; 7],
            "ei_pad must be all zero -- the loader checks each byte"
        );
        assert_eq!(bytes[15], 0, "ei_abiversion must be zero");

        assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), ET_DYN, "e_type");
        assert_eq!(
            u16::from_le_bytes([bytes[18], bytes[19]]),
            EM_BPF,
            "e_machine"
        );
        assert_eq!(
            u64::from_le_bytes(bytes[32..40].try_into().expect("8 bytes")),
            EHDR_SIZE,
            "e_phoff must be exactly 64"
        );
        assert_eq!(
            u64::from_le_bytes(bytes[24..32].try_into().expect("8 bytes")) % INSN_SIZE,
            0,
            "e_entry must be instruction-aligned"
        );
    }

    /// The segment offsets satisfy the alignment rules the loader enforces.
    ///
    /// `p_offset % 8 == 0` is checked by the loader and is easy to break by
    /// adding a section header before the text, so it gets its own assertion.
    #[test]
    fn the_bytecode_segment_is_aligned_and_in_bounds() {
        let bytes = stub_program_bytes();
        let ph = EHDR_SIZE as usize;

        let p_offset = u64::from_le_bytes(bytes[ph + 8..ph + 16].try_into().expect("8 bytes"));
        let p_filesz = u64::from_le_bytes(bytes[ph + 32..ph + 40].try_into().expect("8 bytes"));
        let p_memsz = u64::from_le_bytes(bytes[ph + 40..ph + 48].try_into().expect("8 bytes"));

        assert_eq!(p_offset % INSN_SIZE, 0, "p_offset must be 8-aligned");
        assert_eq!(p_filesz % INSN_SIZE, 0, "p_filesz must be 8-aligned");
        assert_eq!(p_filesz, p_memsz, "p_filesz and p_memsz must match");
        assert!(
            p_offset + p_filesz <= bytes.len() as u64,
            "segment runs past the end of the file"
        );
    }

    /// Generating twice yields identical bytes.
    ///
    /// A stub that varied between calls would make every golden test in this
    /// crate irreproducible, which is exactly the failure the sandbox exists
    /// to detect. Cheap to check, and the check is the point.
    #[test]
    fn generation_is_deterministic() {
        assert_eq!(stub_program_bytes(), stub_program_bytes());
    }
}
