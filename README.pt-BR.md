# GuardWSL

Idioma: [English](README.md)

> Esta tradução é informativa. O [`README.md`](README.md) em inglês é a fonte
> canônica para comportamento, requisitos e limites de segurança.

GuardWSL é uma ferramenta pequena e restrita ao usuário para proteger máquinas
de desenvolvimento com Linux nativo ou WSL2. Ela observa o filesystem local no Linux nativo ou o volume físico do Windows que contém a distribuição WSL2 atual, remove somente artefatos comprovadamente regeneráveis e
não controla comandos de desenvolvimento.

O desenho é deliberadamente conservador: na dúvida, os dados são preservados.

## Status

A versão atual do código é `0.1.1`. `guard --version` mostra o SHA completo
do código-fonte. O texto de `guard status` mantém versão e SHA curto; o JSON
inclui o SHA completo e os dados de instalação. Ainda não há uma versão
pública estável; revise um dry-run antes de ativar limpeza real em qualquer
máquina.

## O que a v1 faz

1. `guard status` mostra a versão do GuardWSL, o SHA curto, a data da
   instalação, a pressão no disco do host e a saúde do monitor. O JSON inclui o
   SHA completo. No WSL2 também mostra o caminho e atributo sparse do VHDX
   atual.
2. Um monitor systemd de usuário executa manutenção por idade e reage à pressão
   no disco do host.
3. Uma allowlist exata permite limpar somente caches e artefatos conhecidos
   após validar proprietário, Git, idade, mount, tipo, hard links, uso por
   processo e identidade.
4. Shims e `guard exec` encaminham builds e demais comandos diretamente.
5. Toda intenção e resultado de limpeza entra em um log JSONL privado.

GuardWSL **não** executa serviço Windows, controla Hyper-V, compacta ou converte
VHDX, encerra o WSL, executa `drop_caches`, limpa Docker, gerencia cgroups ou
instala broker privilegiado.

GuardWSL é independente. Ele não importa configuração, instruções ou políticas
dos repositórios que examina. A descoberta de repositórios serve somente para
provar que um artefato candidato é regenerável e seguro para remoção.

## Início rápido

Requisitos:

- Linux nativo ou WSL2 com systemd habilitado;
- no WSL2, interoperabilidade com Windows PowerShell;
- Rust 1.98.0, Cargo e Bash.

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

## Comandos e configuração

O uso cotidiano é automático. Os comandos existem para inspecionar, diagnosticar, configurar
ou alternar políticas:

```text
guard doctor                           # Verifica a sonda de disco correspondente
guard status                           # Inspeciona pressão no disco do host
guard clean --dry-run                  # Simula a limpeza sem apagar nenhum arquivo
guard clean                            # Executa limpeza segura restrita à allowlist sob demanda
guard config show                      # Exibe a configuração e limites ativos
guard config init                      # Cria ou reinicia ~/.config/guardwsl/config.toml
guard config validate                  # Valida a sintaxe e os limites da configuração
guard history                          # Exibe o histórico de auditoria das limpezas
guard exec -- <comando> [args...]      # Encaminha um comando diretamente
```

### Notas importantes de configuração

- **Comandos de desenvolvimento:** shims e `guard exec` encaminham comandos diretamente, sem fila ou bloqueio.
- **Limites personalizados:** ajuste pressão de disco, raízes e caminhos protegidos em `~/.config/guardwsl/config.toml`.

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

## Comandos de desenvolvimento

O GuardWSL observa a pressão no disco físico para status e decisões de limpeza. Builds e
demais comandos são encaminhados diretamente, sem fila, lock ou preflight.

## Disco do host e VHDX sparse

No Linux nativo, é autoritativo o espaço disponível no filesystem local das raízes configuradas; raízes em filesystems distintos são rejeitadas. No WSL2, o espaço físico livre do Windows é autoritativo. O `df` do guest é apenas
diagnóstico, pois um VHDX ext4 dinâmico pode informar capacidade virtual livre
enquanto o volume físico do Windows está quase cheio.

Os limites de pressão são limitados proporcionalmente à capacidade observada, para que um disco pequeno não permaneça sempre sob pressão. Os valores configurados em bytes continuam sendo limites superiores. A limpeza é limitada pela meta efetiva e nunca bloqueia comandos de desenvolvimento.

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
