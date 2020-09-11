# Chat Importer

This tool can import chat records from your im an store into single sqlite database.

This tool is based on another crate [gchdb](https://github.com/darkskygit/GCHDB) of mine, it provides database read-write abstraction and full-text indexing / retrieval feature based on [tantivy](https://github.com/tantivy-search/tantivy) and [cang-jie](https://github.com/DCjanus/cang-jie).

# Now support & Todo

- [x] PC QQ Lite up to 6.7's Mht backup files (system messages can't parse now)
- [ ] Wechat iOS (need help, welcome pr?)
- [ ] Wechat Android (need help, welcome pr?)
- [x] iMessages / Normal iOS Message 
- [ ] Android Messages

# Usage

Backup your qq chat records into mht files in QQ's chat history manager and don't rename them.

``` sh
cargo run --release -- -o your_qq_number <mht_folder_path>
```

# Contributing

Welcome pull request :)

# License

AGPL3.0
