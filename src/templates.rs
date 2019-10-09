use tera::Tera;

pub fn make_tera() -> Tera {
    dbg!(match Tera::new("templates/**/*") {
        Ok(t) => t,
        Err(e) => {
            log::error!("Template error: {}", e);
            std::process::exit(1);
        }
    })
}
