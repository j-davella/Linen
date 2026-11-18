// No console window, even in debug builds: logs go to ~/.linen/logs.
#![windows_subsystem = "windows"]

fn main() {
    linen_lib::run()
}
