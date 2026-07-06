# LuckCode Project Rules

- Follow the backend implementation standards in [doc/backend-coding-standards.md](doc/backend-coding-standards.md) for all Rust code (architecture, modules, error handling, async, tools, providers, config, storage, permissions, testing).
- Run the configured test command after modifying code.
- Do not create git commits unless the user asks for them.
- Do not read `.env`, private keys, or credentials.
- Do not run `sudo`.
- Do not run destructive infrastructure commands such as `terraform apply` or `terraform destroy`.
- Show shell commands to the user before executing them.
