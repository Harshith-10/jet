const std = @import("std");

pub fn main() !void {
    const allocator = std.heap.page_allocator;
    
    while (true) {
        // Allocate 10MB chunks
        const mem = try allocator.alloc(u8, 10 * 1024 * 1024);
        // Write to the memory to ensure the OS maps physical RAM
        @memset(mem, 1);
    }
}