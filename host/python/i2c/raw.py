#!/usr/bin/env python3
"""Minimal raw I2C helpers built on Linux I2C_RDWR ioctls."""

from __future__ import annotations

import ctypes
import fcntl
import os

I2C_M_RD = 0x0001
I2C_RDWR = 0x0707
CHUNK_SIZE = 32


class I2cMsg(ctypes.Structure):
    _fields_ = [
        ("addr", ctypes.c_uint16),
        ("flags", ctypes.c_uint16),
        ("len", ctypes.c_uint16),
        ("buf", ctypes.c_void_p),
    ]


class I2cRdwrIoctlData(ctypes.Structure):
    _fields_ = [
        ("msgs", ctypes.POINTER(I2cMsg)),
        ("nmsgs", ctypes.c_uint32),
    ]


class RawI2cBus:
    def __init__(self, bus_num: int):
        self.fd = os.open(f"/dev/i2c-{bus_num}", os.O_RDWR)

    def close(self) -> None:
        os.close(self.fd)

    def transfer(self, addr: int, *segments: tuple[int, bytes | int]) -> list[bytes]:
        msg_array = (I2cMsg * len(segments))()
        buffers: list[ctypes.Array[ctypes.c_ubyte]] = []
        read_indices: list[int] = []

        for idx, (flags, payload) in enumerate(segments):
            if flags & I2C_M_RD:
                length = int(payload)
                buf = (ctypes.c_ubyte * length)()
                read_indices.append(idx)
            else:
                data = bytes(payload)
                length = len(data)
                buf = (ctypes.c_ubyte * length)(*data) if length else (ctypes.c_ubyte * 1)()
            buffers.append(buf)
            msg_array[idx] = I2cMsg(
                addr=addr,
                flags=flags,
                len=length,
                buf=ctypes.cast(buf, ctypes.c_void_p),
            )

        ioctl_data = I2cRdwrIoctlData(msgs=msg_array, nmsgs=len(segments))
        fcntl.ioctl(self.fd, I2C_RDWR, ioctl_data)

        out: list[bytes] = []
        for idx in read_indices:
            msg = msg_array[idx]
            buf = ctypes.cast(msg.buf, ctypes.POINTER(ctypes.c_ubyte * msg.len)).contents
            out.append(bytes(buf))
        return out

    def write(self, addr: int, data: bytes) -> None:
        self.transfer(addr, (0, data))

    def read(self, addr: int, length: int) -> bytes:
        return self.transfer(addr, (I2C_M_RD, length))[0]


def open_bus(bus_num: int) -> RawI2cBus:
    return RawI2cBus(bus_num)
