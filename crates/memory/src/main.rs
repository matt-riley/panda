use panda_memory::*;
fn main() -> anyhow::Result<()> {
    let text = read_fixture(
        Files([("hello.txt".into(), "Hello from memory".into())].into()),
        EmptySkills,
        &mut MemoryStore::default(),
    )?;
    println!("{text}");
    Ok(())
}
