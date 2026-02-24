mod dialog;
mod wakeword;

fn main() -> anyhow::Result<()> {
    wakeword::run()
}
