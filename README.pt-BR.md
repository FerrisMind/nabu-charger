# nabu-fastcharge

[English](README.md) | [Русский](README.ru.md) | **Português (Brasil)**

> Esta é uma tradução do [README em inglês](README.md). O texto em inglês é o
> principal: se os dois divergirem, vale o inglês.

Driver de carregamento para o **Xiaomi Pad 5 (nabu, Snapdragon 860) no Windows on
ARM64**.

No Windows o tablet não carrega com nenhuma fonte — nem pela USB-A (Quick Charge),
nem pela USB-C (Power Delivery). A causa não é a "falta de carregamento rápido": é
que **o PMIC executa a detecção de adaptador em hardware (APSD), mas nenhum
componente do Windows lê o resultado dela**, então o limite de corrente de entrada
nunca sobe e a carga não começa.

O driver fecha exatamente essa lacuna: lê o resultado da detecção e aplica a
política de corrente.

A parte reutilizável deste trabalho não é o driver, e sim o que se descobriu sobre o
hardware: [docs/FINDINGS.md](docs/FINDINGS.md) reúne seis achados sobre o carregamento
nesta plataforma, cada um com o código ou a medição em que se apoia. Comece por ali se
você estiver portando o Windows para um tablet da classe nabu, e não usando este driver.

## Estrutura

| Crate | O que é | Verificação |
|---|---|---|
| [`crates/core`](crates/core) | núcleo da lógica SMB: detecção APSD, política de corrente, estados, timeouts, journal. Sem `std`, sem `unsafe` | 37 testes unitários |
| [`crates/ln8000`](crates/ln8000) | núcleo do driver da bomba de carga LN8000 (I²C 0x51): registradores, modos, proteções, ADC, telemetria de sessão, proteção térmica. Sem `std`, sem `unsafe` | 125 testes unitários, 6 testes de integração, 2 doctests |
| [`crates/spb`](crates/spb) | tipos SPB e montagem da lista de transferências compartilhados pelos drivers em modo kernel; declarados à mão porque o `wdk-sys` não os gera | 10 testes unitários |
| [`crates/ln8000-kmdf`](crates/ln8000-kmdf) | driver KMDF do LN8000 no nó ACPI `PEIC` sobre I²C (SPB / Resource Hub), além de scripts de instalação e diagnóstico | compila para ARM64 |
| [`crates/host`](crates/host) | camada host: transportes (mock, TCP), journal JSONL, `tracing`, simulador de dispositivo, benchmarks | 9 testes de integração, 2 doctests |
| [`crates/cli`](crates/cli) | a ferramenta `nabu-charger`: `demo`, `detect`, `sim`, `pump`, `verify` | 4 testes de CLI |
| [`crates/kmdf`](crates/kmdf) | driver em modo kernel (KMDF) para ARM64 pelo barramento SPMI | compilado: `kmdf.sys` ARM64, assinado, `infverif` aprovado |

O `cargo test --workspace` cobre 195 testes — unitários, de integração e doctests —
nos cinco crates do workspace raiz. Os dois drivers em modo kernel declaram o
próprio workspace, porque precisam do WDK e do `cargo-wdk`, que o ambiente normal de
CI não tem.

## Início rápido

```powershell
# 1. As mesmas verificações que a CI executa
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 2. Demonstração: todos os tipos de adaptador no transporte mock, mais o journal
cargo run -p cli -- --journal artifacts/journal-demo.jsonl demo

# 3. A bomba de carga LN8000 em um barramento I²C mock (configurar -> modo 2:1 -> status -> ADC)
cargo run -p cli -- pump --profile qc35

# 4. Autoverificação das tabelas e da aritmética do driver
cargo run -p cli -- verify

# 5. Uma bancada sem hardware: o simulador de dispositivo mais o transporte TCP real
cargo run -p cli -- sim --adapter hvdcp3 --listen 127.0.0.1:9700
cargo run -p cli -- detect --transport tcp --addr 127.0.0.1:9700
```

Saída do `pump` no barramento mock. O mock é determinístico, então esta saída se
reproduz exatamente:

```text
bus           : mock
state         : probed
after configuration: configured
mode          : SWITCHING (code 3)
SYS_STS       : 0x04 (current loop: no, voltage loop: no)
faults        : none

ADC readings (mock), alarm channels:
  iin       ADC1     489000 uA
  vin       ADC3     192000 uV
  vbat      ADC6    3340000 uV

operations    : writes 33, reads 84
after standby : STANDBY
```

Saída do `demo`. As duas falhas são intencionais: elas exercitam o caminho de erro.

```text
scenario   result adapter    current,µA pump   note
------------------------------------------------------------------------------
HVDCP3P5   ok     HVDCP3     3000000 yes    Quick Charge 3.0, 9 V and 3 A, charge pump possible
HVDCP3     ok     HVDCP3     3000000 yes    Quick Charge 3.0, 9 V and 3 A, charge pump possible
HVDCP2     ok     HVDCP2     1500000 no     Quick Charge 2.0, 9 V and 1.5 A
DCP        ok     DCP        1500000 no     charging-only port, BC1.2 1.5 A
SDP        ok     SDP        500000  no     standard USB port, 500 mA limit
DETACHED   failure -          -       -      error detection_timeout - no power: a timeout is expected
UNKNOWN    failure -          -       -      error unknown_adapter_pattern - unknown pattern: a failure is expected

journal: artifacts\journal-demo.jsonl
```

O programa imprime em inglês, então as saídas acima estão transcritas literalmente,
sem tradução.

O `verify` termina com a linha:

```text
self-check: passed (policies, current grid, APSD decoding, error path)
```

## Como funciona

```text
cliente (IOCTL) ──► driver KMDF ──► núcleo da lógica ──► transporte ──► \Device\RESOURCE_HUB (SPMI) ──► SMB no PM8150B
                                     │
                                     ├─ lê APSD_STATUS / APSD_RESULT_STATUS
                                     ├─ decodifica o tipo de adaptador (tabela do Android)
                                     └─ escreve o limite de corrente de entrada e a tensão do QC2
```

O núcleo não sabe nada sobre o Windows nem sobre E/S: ele funciona sobre o trait
[`ChargerTransport`](crates/core/src/transport.rs) e recebe o tempo e o journal de
fora. Por isso toda a lógica, incluindo timeouts e recuperação de falhas, é
testável sem hardware.

Mais detalhes: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md),
[docs/REGISTERS.md](docs/REGISTERS.md), [docs/LN8000.md](docs/LN8000.md).

## Limitações e ressalvas honestas

* **A superfície de IOCTL do driver SMB ainda não está ligada.** O `GET_STATUS`
  responde. O `READ_REG` devolve uma estrutura cujo `error_code` é
  `STATUS_NOT_IMPLEMENTED`, e `WRITE_REG`, `SET_ICL` e `GET_JOURNAL` devolvem
  `STATUS_NOT_IMPLEMENTED` diretamente. A detecção e a aplicação da política rodam
  pelo timer de bring-up, então `DETECT_START` e `APPLY_POLICY` respondem do mesmo
  jeito. A lógica por trás desses pontos de entrada está escrita e coberta por
  testes com mock; o que falta é a ligação no lado do kernel.
* **O layout da resposta do barramento SPMI não foi confirmado por engenharia
  reversa.** O transporte monta a leitura de um registrador como "endereço de 16
  bits, depois um byte", e o driver mantém um `Charger` vivo sobre ele, mas o
  enquadramento da resposta do barramento não foi provado, então
  [docs/REGISTERS.md](docs/REGISTERS.md) e
  [docs/SPMI-PATH.md](docs/SPMI-PATH.md) mantêm as leituras de registrador como
  provisórias. Um palpite não é apresentado como fato.
* **Compilar os drivers em modo kernel exige o WDK.** O `kmdf.sys` é assinado, o
  INF dele passa no `infverif`, e o campo de máquina do PE confirma ARM de 64 bits
  (`0xAA64`). O pacote é gravado em `artifacts/driver-arm64/`. É preciso LLVM
  **17.x** para o `bindgen`.
* **A negociação de tensão (PD) continua com a parte Type-C da plataforma.** Sem
  ela a bomba de carga pode manter uma tensão já negociada ou trabalhar em bypass a
  partir de 5 V, mas não entrega os 33 W completos.
* **O driver do LN8000 está implantado e em medição, não concluído.** O driver KMDF
  para o nó ACPI `PEIC` (endereço I²C 0x51) está compilado e instalado no tablet.
  [docs/STATE-2026-09-17.md](docs/STATE-2026-09-17.md) registra os defeitos ainda
  abertos, e [docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md) é o procedimento de
  instalação e diagnóstico.

## Compilando o driver

```powershell
rustup target add aarch64-pc-windows-msvc
cargo install cargo-wdk --locked
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # exige LLVM 17.0.6

# O driver de detecção SMB (detecção do bloco e limite de corrente de entrada)
cd crates/kmdf        ; cargo wdk build --target-arch arm64 --profile release

# O driver da bomba de carga LN8000 (nó PEIC, I2C 0x51)
cd crates/ln8000-kmdf ; cargo wdk build --target-arch arm64 --profile release
```

Os pacotes são gravados em `artifacts/driver-arm64/` (SMB) e
`artifacts/driver-ln8000-arm64/` (LN8000). Ambos são gerados localmente e não são
versionados no git.

Instalação e diagnóstico do LN8000 — [docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md):
`install-driver.ps1`, `nabu-ln8000.ps1 status|sessions|read|write|journal`,
`run-acceptance.ps1` (o protocolo de aceitação automatizado) e
`uninstall-driver.ps1`. Um script PowerShell que contém texto não ASCII é salvo em
UTF-8 com BOM, porque o PowerShell 5.1, caso contrário, o decodifica como ANSI e
quebra a análise das aspas.

## Licença e política de acesso

**Licença: GPL-2.0-or-later** ([LICENSE](LICENSE)). A lógica do LN8000 é um porte do
driver GPL-2.0-or-later do kernel Android para o mesmo chip, então este repositório não
pode ser MIT/Apache. A origem de cada parte está em [PROVENANCE.md](PROVENANCE.md).

**Política de acesso: a bomba de carga é acessível apenas a `LocalSystem` e
administradores.** O driver aplica esse descritor ao objeto de dispositivo e o INF aplica
o mesmo ao nó do dispositivo, porque todos os códigos de controle são `FILE_ANY_ACCESS` e
o driver não verifica quem está chamando. Por isso os scripts em `deploy/` precisam de
privilégio elevado; ler as marcas de telemetria não precisa, pois são valores de
registro.
