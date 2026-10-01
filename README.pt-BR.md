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
inclui o SHA completo e os dados de instalação. Os arquivos de release
incluem `SHA256SUMS` e a árvore de instalação completa. Ainda não há uma
versão pública estável; revise um dry-run antes de ativar limpeza real em
qualquer máquina.

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
- Bash. Um release pré-compilado não precisa de Rust; build a partir do
  código-fonte precisa de Rust 1.98.0 e Cargo.

### Instalar a partir de um release (recomendado para clientes)

```bash
# Baixe o release e o checksum, e verifique antes de extrair.
curl -fLO "https://github.com/emersonbusson/guardwsl/releases/latest/download/guardwsl-VERSION-x86_64-unknown-linux-gnu.tar.gz"
curl -fLO "https://github.com/emersonbusson/guardwsl/releases/latest/download/SHA256SUMS"
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf guardwsl-VERSION-x86_64-unknown-linux-gnu.tar.gz
cd guardwsl-VERSION-x86_64-unknown-linux-gnu
./scripts/install-linux.sh
```

Substitua `VERSION` pela versão baixada (por exemplo `v0.1.1`). O instalador
usa o binário incluído e não precisa de Cargo nem de checkout Git.

### Instalar a partir do código-fonte

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
guard --version
guard doctor
guard status
guard clean --dry-run
```

## Comandos e configuração

O uso cotidiano é automático. Os comandos existem para inspecionar, diagnosticar, configurar
ou alternar políticas:

```text
guard --version                        # Mostra a versão e o SHA completo do código-fonte
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

- caches npm, Yarn (classic e Berry), pnpm (cache e store), Bun, Cargo e Go;
- caches de ferramentas compartilhadas (`sccache`, `vscode-cpptools`, navegadores Playwright);
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

## Auditoria de referências no workspace

Quando este checkout estiver dentro de um workspace com vários repositórios,
execute a auditoria somente leitura:

    python3 scripts/audit-workspace-references.py

Ela verifica os arquivos dos repositórios irmãos e preserva as exceções
documentadas do Vitae e do README público do perfil. Consulte
[Auditoria de referências no workspace](docs/WORKSPACE_REFERENCE_AUDIT.md)
para conhecer o escopo e os códigos de saída. O CI hospedado deste repositório
não possui os checkouts irmãos para verificar.

## Solução de problemas

- **`guard status` mostra `Host: unavailable`.** No WSL2, verifique se a
  interoperabilidade com Windows PowerShell funciona
  (`powershell.exe -NoProfile -Command 'echo ok'`). No Linux nativo, confira
  se toda entrada de `scan_roots` existe e está em um único filesystem.
  `guard doctor` nomeia a verificação que falhou.
- **O disco continua cheio depois da limpeza.** O GuardWSL reporta bytes
  lógicos removidos separadamente do espaço físico livre no host. Um VHDX
  sparse não encolhe a cada exclusão, e o `fstrim` do WSL frequentemente
  devolve quase nada. Confie no espaço livre do host em `guard status`, não
  no `df` do guest.
- **A limpeza removeu menos do que o esperado.** Janelas de idade, o orçamento
  de ações por ciclo e os skips de segurança limitam cada execução.
  `guard history` mostra o que foi tentado e por que um candidato foi pulado.
  Dry-runs nunca removem.
- **`sha256sum -c SHA256SUMS` falha.** Baixe os dois arquivos de novo; não
  instale um release que não verifique.
- **O instalador pede Cargo.** Você está em um checkout de código-fonte.
  Instale o Rust 1.98.0 ou use um tarball de release, que já inclui o binário.

## Desenvolvimento

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
bash tests/install_platform.sh tests/install_order.sh
cargo audit --deny warnings
cargo deny check
bash -n scripts/install-linux.sh scripts/install-shims.sh
```

Testes destrutivos usam somente diretórios temporários isolados. Eles nunca
alteram WSL, Windows, Hyper-V ou dados reais de projetos.

Consulte [Arquitetura](docs/ARCHITECTURE.md),
[Configuração](docs/CONFIGURATION.md), [Processo de release](docs/RELEASE.md),
[Contribuição](CONTRIBUTING.md) e [Política de segurança](SECURITY.md). Esses
documentos são canônicos em inglês.

## Licença

GuardWSL é licenciado, à escolha do usuário, sob Apache License 2.0 ou MIT
License. Consulte `LICENSE-APACHE` e `LICENSE-MIT`.
