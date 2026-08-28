# GuardWSL

Idioma: [English](README.md)

> Esta tradução é informativa. O [`README.md`](README.md) em inglês é a fonte
> canônica para comportamento, requisitos e limites de segurança.

GuardWSL é uma ferramenta pequena e restrita ao usuário para proteger máquinas
de desenvolvimento com WSL2. Ela observa o volume físico do Windows que contém
a distribuição atual, remove somente artefatos comprovadamente regeneráveis e
impede que builds pesados reconhecidos comecem ao mesmo tempo.

O desenho é deliberadamente conservador: na dúvida, os dados são preservados.

## Status

A versão atual do código é `0.1.0`. Seus 50 testes Rust, formatter, Clippy,
sintaxe shell, auditoria de dependências e política de dependências passam
localmente. Ainda não há uma versão pública estável; revise um dry-run antes de
ativar limpeza real em qualquer máquina.

## O que a v1 faz

1. `guard status` mostra o disco físico do host, a RAM física do Windows, o
   caminho e atributo sparse do VHDX atual, a saúde do monitor e o gate de
   builds.
2. Um monitor systemd de usuário executa manutenção por idade e reage à pressão
   no volume físico do host.
3. Uma allowlist exata permite limpar somente caches e artefatos conhecidos
   após validar proprietário, Git, idade, mount, tipo, hard links, uso por
   processo e identidade.
4. Shims cooperativos serializam builds pesados reconhecidos. Testes, lint,
   typecheck, checks, e2e e instalações sempre executam diretamente.
5. Toda intenção e resultado de limpeza entra em um log JSONL privado.

GuardWSL **não** executa serviço Windows, controla Hyper-V, compacta ou converte
VHDX, encerra o WSL, executa `drop_caches`, limpa Docker, gerencia cgroups ou
instala broker privilegiado.

GuardWSL é independente. Ele não importa configuração, instruções ou políticas
dos repositórios que examina. A descoberta de repositórios serve somente para
provar que um artefato candidato é regenerável e seguro para remoção.

## Início rápido

Requisitos:

- WSL2 com systemd habilitado;
- interoperabilidade com Windows PowerShell;
- Rust 1.98.0, Cargo, Bash e `flock`;
- disco e RAM físicos suficientes para o build de instalação.

Revise o instalador antes de executá-lo:

```bash
git clone https://github.com/emersonbusson/guardwsl.git
cd guardwsl
./scripts/install-linux.sh
```

O instalador é transacional: ele salva todos os arquivos gerenciados do usuário
e restaura o estado anterior se o serviço não ficar saudável. A lista exata
está em [Instalação e remoção](docs/INSTALLATION.md) — documento canônico em
inglês.

Verifique sem apagar nada:

```bash
guard doctor
guard status
guard clean --dry-run
```

## Comandos

```text
guard doctor
guard status
guard clean --dry-run
guard clean
guard admission status
guard admission on
guard admission off
guard config show
guard config init
guard config validate
guard history
guard exec -- <comando> [argumentos...]
```

O uso cotidiano é automático. `guard admission off` desliga somente a
serialização de builds pesados; o preflight de disco/RAM e a segurança da
limpeza continuam ativos. Testes e checks permanecem diretos nos dois modos.

## Escopo exato da limpeza

A allowlist da v1 contém:

- caches npm, Yarn, pnpm, Cargo e Go;
- diretórios Rust `target`;
- `.next`, `.turbo`, `.vite`, `.pytest_cache`, `.mypy_cache` e `.ruff_cache`;
- `node_modules` quando um lockfile reconhecido prova reprodutibilidade.

Diretórios genéricos `dist`, `build` e `out` nunca são removidos. Código-fonte,
`.git`, configurações, segredos, bancos, uploads, mídia, dados Docker e caminhos
desconhecidos nunca são candidatos.

A configuração padrão descobre repositórios Git sob o diretório home do usuário
atual. Todas as raízes são configuráveis, e caminhos protegidos são verificados
antes de qualquer planejamento. Configurações novas protegem diretórios comuns
de credenciais e controle, como `.ssh`, `.gnupg`, `.config`, `.aws`, `.azure`,
`.kube`, keyrings, password stores e volumes Docker.

Leia o [modelo de segurança](docs/SAFETY.md) antes de ativar limpeza real.

## Coordenação de builds pesados

O preflight padrão exige 64 GiB livres no volume físico do WSL e 12 GiB de RAM
física disponível no Windows: piso de 8 GiB para o host e 4 GiB adicionais para
o build. Os valores ficam em `~/.config/guardwsl/config.toml`; consulte a
[referência canônica de configuração](docs/CONFIGURATION.md), em inglês.

O gate é cooperativo. Os pontos de entrada normais são cobertos pelos shims, mas
um caminho executável absoluto fora deles pode contornar o Guard. O kernel
libera os locks quando os processos terminam; não existe fila distribuída ou
serviço de leases.

## Disco físico e VHDX sparse

O espaço físico livre do Windows é autoritativo. O `df` do Linux é apenas
diagnóstico, pois um VHDX ext4 dinâmico pode informar capacidade virtual livre
enquanto o volume físico do Windows está quase cheio.

`sparseVhd=true` na `.wslconfig` vale automaticamente para VHDs novos; isso não
prova que um VHDX existente está sparse. GuardWSL consulta o atributo real e
separa bytes lógicos removidos da variação física observada no host.

GuardWSL nunca converte ou compacta VHDX. A conversão de disco existente é uma
operação administrativa offline, com WSL parado e backup verificado.

## Desenvolvimento

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo audit --deny warnings
cargo deny check
bash -n scripts/install-linux.sh scripts/install-shims.sh
```

Testes destrutivos usam somente diretórios temporários isolados. Eles nunca
alteram WSL, Windows, Hyper-V ou dados reais de projetos.

Consulte [Arquitetura](docs/ARCHITECTURE.md),
[Configuração](docs/CONFIGURATION.md), [Contribuição](CONTRIBUTING.md) e
[Política de segurança](SECURITY.md). Esses documentos são canônicos em inglês.

## Licença

GuardWSL é licenciado, à escolha do usuário, sob Apache License 2.0 ou MIT
License. Consulte `LICENSE-APACHE` e `LICENSE-MIT`.
