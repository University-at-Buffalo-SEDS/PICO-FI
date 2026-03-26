#!/usr/bin/env python3
"""
Simple I2C chunking test - validates the 32-byte chunking logic
"""

FRAME_SIZE = 258
CHUNK_SIZE = 32

def test_chunking():
    """Test that we can chunk 258 bytes into 32-byte pieces"""
    test_data = bytes(range(256)) + b'\x00\x02'  # 258 bytes
    
    # Simulate sending in chunks
    sent_chunks = []
    for i in range(0, FRAME_SIZE, CHUNK_SIZE):
        chunk = list(test_data[i:i+CHUNK_SIZE])
        if chunk:
            sent_chunks.append(len(chunk))
    
    print(f"Total data: {FRAME_SIZE} bytes")
    print(f"Chunk size: {CHUNK_SIZE} bytes max")
    print(f"Chunks needed: {len(sent_chunks)}")
    print(f"Chunk sizes: {sent_chunks}")
    print(f"Total sent: {sum(sent_chunks)} bytes")
    
    # Simulate receiving in chunks
    received = bytearray()
    for i in range(0, FRAME_SIZE, CHUNK_SIZE):
        chunk_size = min(CHUNK_SIZE, FRAME_SIZE - len(received))
        # Simulate receiving chunk_size bytes
        chunk = test_data[i:i+chunk_size]
        received.extend(chunk)
    
    print(f"Total received: {len(received)} bytes")
    print(f"Data match: {bytes(received) == test_data}")
    
    if bytes(received) == test_data:
        print("\n✅ Chunking logic is correct!")
        return True
    else:
        print("\n❌ Chunking logic failed!")
        return False

if __name__ == "__main__":
    success = test_chunking()
    exit(0 if success else 1)

