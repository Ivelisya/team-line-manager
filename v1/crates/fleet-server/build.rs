// sqlx::migrate! 在编译时把 migrations/ 嵌进二进制：只加了迁移文件、没改代码时也要重编（sqlx 文档推荐的写法）
fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
