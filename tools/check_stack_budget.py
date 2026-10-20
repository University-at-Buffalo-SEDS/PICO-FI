#!/usr/bin/env python3
"""Validate fixed RP2040 task stack frames from the linked Thumb prologues.

Catches oversized async task poll frames without a nightly compiler or extra
GCC tools. The allowance includes additional call/interrupt headroom; this is
not a complete call-graph stack proof. Physical testing is still required.
"""
import argparse
import pathlib
import re
import struct


def thumb_frame(code, address, read_word):
    """Decode stack allocation in LLVM's Cortex-M0+ fixed-frame prologue.

    Unknown register-based SP allocation fails closed. Stop at control flow,
    before epilogue restores or literal pools can be interpreted as code.
    """
    registers = {}
    size = 0
    for offset in range(0, min(len(code), 64), 2):
        op = struct.unpack_from("<H", code, offset)[0]
        pc = address + offset
        if op & 0xFE00 == 0xB400:  # PUSH low registers and optional LR
            size += ((op & 255).bit_count() + ((op >> 8) & 1)) * 4
        elif op & 0xFF80 == 0xB080:  # SUB SP, #imm7*4
            size += (op & 127) * 4
        elif op & 0xF800 == 0x4800:  # LDR low register, [PC,#imm8*4]
            registers[(op >> 8) & 7] = read_word(((pc + 4) & ~3) + (op & 255) * 4)
        elif op & 0xFF00 == 0x4400:  # ADD high register, register
            dst = (op & 7) | ((op >> 4) & 8)
            if dst == 13:
                src = (op >> 3) & 15
                if src not in registers:
                    raise ValueError("unknown register-based SP allocation")
                value = registers[src]
                signed = value if value < 0x80000000 else value - 0x100000000
                if signed >= 0: break  # epilogue
                size -= signed
        elif op & 0xFF80 == 0xB000: break  # ADD SP immediate (epilogue)
        elif op & 0xFF00 in (0xBD00, 0xBC00): break
        elif op & 0xFF00 == 0x4700: break  # BX / BLX
        elif op & 0xF000 == 0xD000 or op & 0xF800 == 0xE000: break
        elif op & 0xF800 == 0xF000 and offset + 4 <= len(code):
            second = struct.unpack_from("<H", code, offset + 2)[0]
            if second & 0xD000 == 0xD000: break  # BL
    return size


def elf_task_frames(path):
    data = pathlib.Path(path).read_bytes()
    if data[:6] != b"\x7fELF\x01\x01": raise ValueError("requires little-endian ELF32")
    shoff = struct.unpack_from("<I", data, 32)[0]
    stride, count, _ = struct.unpack_from("<HHH", data, 46)
    sections = [struct.unpack_from("<10I", data, shoff+i*stride) for i in range(count)]
    def content(s): return data[s[4]:s[4]+s[5]]
    def string(blob, start): return blob[start:blob.index(0,start)].decode()
    def read(address, count):
        for section in sections:
            if section[1] == 1 and section[3] <= address and address+count <= section[3]+section[5]:
                start = section[4] + address-section[3]
                return data[start:start+count]
        raise ValueError(f"address {address:#x} outside ELF sections")
    symbols = {}; frames = {}
    for section in sections:
        if section[1] != 2: continue
        strings = content(sections[section[6]])
        for pos in range(section[4],section[4]+section[5],section[9]):
            name,address,size,info,_,_ = struct.unpack_from("<IIIBBH",data,pos)
            label = string(strings,name); symbols[label] = address
            if info & 15 == 2:
                address &= ~1
                try:
                    frames[label] = thumb_frame(read(address,min(size,64)),address,
                        lambda a: struct.unpack('<I',read(a,4))[0])
                except ValueError:
                    if 'uart_core_task' in label and '4poll' in label: raise
                    # Assembly helper epilogues need not use LLVM's fixed-frame
                    # pattern. The UART task itself must always be decoded.
                    continue
    if not frames: raise ValueError("missing linked task poll prologues")
    return symbols, frames


def validate_frame(frame, capacity, label, margin=8192):
    if frame + margin > capacity:
        raise ValueError(f"{label}: compiler frame {frame} + {margin} byte headroom exceeds {capacity} byte stack")


def check(path, source):
    symbols, frames = elf_task_frames(path)
    match = re.search(r"const CORE1_STACK_BYTES: usize = (\d+) \* 1024;",pathlib.Path(source).read_text())
    if not match: raise ValueError("cannot locate core 1 stack reservation")
    uart = [size for name,size in frames.items() if 'uart_core_task' in name and '4poll' in name]
    main = [size for name,size in frames.items() if 'uart_core_task' not in name]
    if not main: raise ValueError("missing compiled Ethernet task frame")
    capacity = int(match[1])*1024
    if uart: validate_frame(max(uart),capacity,"UART core 1")
    main_space = symbols['_stack_start'] - symbols['__ebss']
    validate_frame(max(main),main_space,"Ethernet core 0")
    print(f"Stack guard: UART frame={max(uart,default=0)} reserved={capacity}; main largest linked frame={max(main)} available={main_space}")


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('elf'); args=parser.parse_args()
    check(args.elf,pathlib.Path(__file__).resolve().parents[1]/'src/main.rs')
