fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    println!("outside {}", klib::outside());
    println!("inside {}", klib::inside());
    println!("generated {}", klib::generated());
    println!("script_env {}", klib::script_env());
    println!("script_cfg {}", klib::script_cfg());
    println!("feature {}", klib::feature());
    println!("process_env {}", klib::process_env());
    println!("config_env {}", klib::config_env());
    println!("flag_cfg {}", klib::flag_cfg());
    println!("assertions {}", klib::assertions());
    println!("overflows {}", klib::overflows(255));
    println!("scaled {}", klib::scaled(5));
    println!("shape {}", klib::shape());
    println!("described {}", klib::described());
}
