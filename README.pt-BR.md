# nabu-fastcharge

[English](README.md) | [Русский](README.ru.md) | **Português (Brasil)**

![Status: prévia experimental](https://img.shields.io/badge/status-experimental%20preview-orange?style=for-the-badge) ![Dispositivo: Xiaomi Pad 5](https://img.shields.io/badge/device-Xiaomi%20Pad%205-blue?style=for-the-badge) ![Plataforma: Windows 11 ARM64](https://img.shields.io/badge/platform-Windows%2011%20ARM64-0078D4?style=for-the-badge)

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

## Estado do projeto

Última versão: **0.3.1**, com o driver **20.47.10.672** — o pacote ARM64 instalável está
anexado a ela (`nabu-ln8000-driver-0.3.1-arm64.zip`). É esse pacote que está instalado no
tablet de desenvolvimento (`oem166.inf`, nó do dispositivo em `OK` / `CM_PROB_NONE`), e as
medições abaixo foram feitas nele: uma fonte Quick Charge negociou até 8.3 V, `Iin`
0.23–1.28 A, carga 196 → 199, o sinalizador de carregamento 338 ms após o veredito e a
remoção liberada em 1.1 s.

A versão anterior, **0.3.0** (driver 20.47.10.665), introduziu a política de acesso
verificada nela — um processo sem elevação é recusado com `ERROR_ACCESS_DENIED` (5).

**Resumo:** ✅ carregamento rápido com uma fonte Quick Charge · ✅ telemetria ao vivo da
bomba · ⚠️ uma fonte Power Delivery ainda não aumenta a carga da bateria · ⚠️ a queda do
veredito de CA por trás do reset do brilho — a liberação passou a três ticks (750 ms) e foi
medida em 1.1 s a partir da remoção do cabo, mas a rota do veto de duplicação, que lê uma
bomba que saiu da comutação como "sem fonte", continua aberta

Cada linha abaixo se apoia em uma medição feita no tablet, não em um teste que passou.
Onde um defeito está marcado como não corrigido, uma captura ao vivo mostra o defeito
acontecendo: as evidências estão em [docs/FINDINGS.md](docs/FINDINGS.md) e na lista de
defeitos mais adiante.

### Status resumido

| Recurso | Notas | Status |
|---|---|---|
| 🔌 Carregamento rápido com fonte Quick Charge / HVDCP | Medido: SoC 44 → 85 %, `Iin` 1,13–1,62 A, `Vin` 8,7–9,4 V, bomba em modo 2:1 | ✅ |
| 🔎 Detecção de adaptador (APSD) e política de corrente | Tabelas de decodificação e limites por tipo de adaptador, conferidos com o driver Android: 80 verificações, 0 divergências | ✅ |
| ⚙️ Núcleo da bomba: registradores, modos, proteções, ADC, proteção térmica | 125 testes unitários, 6 de integração, 2 doctests; conferido com o cabeçalho do fabricante | ✅ |
| 📈 Telemetria ao vivo e journal de sessão | `Vin`, `Iin`, `VBAT`, temperatura do cristal, falhas e modo, lidos da bomba por I²C no tablet | ✅ |
| 📦 Pacote ARM64 assinado, build reproduzível, rollback | Verificação do pacote 35/35, dos fontes 80/80, rollback verificado no tablet | ✅ |
| 🎛 Limites e perfil de proteção pelo registro | Alterá-los exige uma escrita no registro e reiniciar o dispositivo, não recompilar | ✅ |
| 🔋 Fonte Power Delivery (USB-C) | O driver obtém CA, mas a bateria não ganha carga; a negociação acima de 5 V fica com a parte Type-C da plataforma, então os 33 W completos ficam fora de alcance | ⚠️ |
| 🖥 A superfície de IOCTL do driver SMB | `GET_STATUS` responde; `READ_REG`, `WRITE_REG`, `SET_ICL`, `GET_JOURNAL`, `DETECT_START`, `APPLY_POLICY` retornam `STATUS_NOT_IMPLEMENTED`. A lógica por trás deles está escrita e testada com mock; a ligação no kernel não | ⚠️ |
| 🔬 Enquadramento da resposta SPMI | Não confirmado por engenharia reversa, então as leituras de registrador seguem provisórias | ⚠️ |
| 💡 Queda do veredito de CA / reset do brilho | **Não corrigida.** Uma captura ao vivo mostra CA → CC → CA em 2,647 s com o cabo imóvel e a bomba ociosa | ❌ |
| 📱 Outros dispositivos SM8150 | Só o Xiaomi Pad 5 foi testado; o driver se associa se o nó existir em I²C 0x51 | ⚠️ |
| 🧩 Um dispositivo com LN8000 mas sem o nó `PEIC` na DSDT | Não suportado — exige alteração de ACPI | ❌ |

### Defeitos conhecidos, não corrigidos

Todos são achados ao vivo ou no código, e cada um tem reprodução; não há suposições
aqui.

**O veredito de CA cai com a fonte conectada** — é o que importa, e a correção
implantada não o cobre. Quando a bomba sai do modo 2:1, `Iin` fica no piso de 39 mA do
ADC e `Vin` recua para `2 · VBAT`, que é o ponto de operação *normal* de uma bomba 2:1,
não evidência de ausência. O veto de VBUS dobrado em `online_raw` lê isso como "sem
adaptador", a retenção de 8 s expira, e o Windows vê uma troca de fonte de energia — é
assim que acontece o reset do brilho relatado. Medido no tablet com o cabo imóvel:
CA → CC → CA em 2,647 s, com o bit 4 de `Fault1Sts` *limpo*, ou seja, o próprio detector
de VBUS do hardware dizia que o cabo estava lá, e com todas as leituras plenamente
utilizáveis, então o novo ramo `held` nunca foi alcançável.

| Defeito | O que ele faz |
|---|---|
| O CA chega segundos depois do cabo | O veredito (`OnlineRaw`) está no primeiro tick, mas o Windows lê o *seguinte* e a subida da bomba bloqueia o tick: medidos **8,7 s** da inserção até `pwr = 1` numa fonte Quick Charge e 5,6 s numa fonte de 5 V simples. Correção planejada, fora desta versão |
| A temperatura do cristal é publicada com o ADC hibernando | O bit 1 de `AdcValid` é reportado para um canal adormecido, então **160,0 °C** é publicado e todo consumidor o imprime fielmente |
| O VBAT do LN8000 lê baixo | 42–43 mV abaixo do medidor de combustível, e esse canal alimenta o portão do modo 2:1 |
| `EngageState` discorda de `SuMode` | Publica 4 (NO_HEADROOM) enquanto `SuMode` fica em 3 (switching); ruído apenas na marca |

Duas falhas de hardware deste tablet não têm relação com o driver, mas aparecem na
telemetria dele: o nó PMIC TCC `ACPI\QCOM0582` está em estado de erro, e o `WUDFRd`
falha ao carregar 48 vezes para a plataforma de sensores `ACPI\QCOM059F`.

As ressalvas de build e de ferramentas — a exigência do WDK, a versão do LLVM, o que
fica com a plataforma — estão em
[Limitações e ressalvas honestas](#limitações-e-ressalvas-honestas).

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
  Os defeitos abertos estão na seção de estado do projeto acima e em
  [docs/FINDINGS.md](docs/FINDINGS.md); [docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md)
  é o procedimento de instalação e diagnóstico.
  [docs/STATE-2026-09-17.md](docs/STATE-2026-09-17.md) é um retrato datado, anterior
  ao caminho de acesso funcional - leia-o pelo que foi descartado, não pelo estado
  atual.

## Compilando e instalando o driver

A versão curta está abaixo. O procedimento completo, a ferramenta de diagnóstico, os
perfis de configuração, a tabela de falhas e o registro de riscos estão em
[docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md).

### O que é necessário

| Requisito | Versão / observação |
|---|---|
| Rust | O canal é fixado pelo `rust-toolchain.toml`; adicione o alvo com `rustup target add aarch64-pc-windows-msvc` |
| Windows Driver Kit | WDK 10.0.26100 — a compilação com `cargo wdk` precisa dele |
| LLVM / libclang | **17.0.6**. É o que gera as ligações do `bindgen`, e a 23.x quebra a compilação. Aponte `LIBCLANG_PATH` para `C:\Program Files\LLVM\bin` |
| cargo-wdk | `cargo install cargo-wdk --locked` |
| O tablet | Windows 11 ARM64 com **assinatura de teste ligada e Secure Boot desligado**. Os drivers são assinados com certificado de teste, e sem isso o Windows não os carrega |
| Permissões | Administrador no tablet para a instalação e para a ferramenta de diagnóstico — o objeto de dispositivo do driver é restrito a `LocalSystem` e Administradores |

### Compilação

```powershell
rustup target add aarch64-pc-windows-msvc
cargo install cargo-wdk --locked
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # exige LLVM 17.0.6

# O driver de detecção SMB (detecção do bloco e limite de corrente de entrada)
cd crates/kmdf        ; cargo wdk build --target-arch arm64 --profile release

# O driver da bomba de carga LN8000 (nó PEIC, I2C 0x51)
cd crates/ln8000-kmdf ; cargo wdk build --target-arch arm64 --profile release
```

O `deploy/build-arm64.ps1` compila os dois, assina-os e grava as somas de verificação e o
manifesto da compilação. Os pacotes são gravados em `artifacts/driver-arm64/` (SMB) e
`artifacts/driver-ln8000-arm64/` (LN8000). Ambos são gerados localmente e não são
versionados no git. O LN8000 é o que tem procedimento de instalação; o driver SMB compila
para ARM64, mas os IOCTLs dele são stubs e ele não foi implantado no tablet — a tabela de
status acima diz quais respondem.

`deploy/assemble-release.ps1 -Version <x.y.z>` empacota o kit instalável no arquivo de
release. Todo o procedimento — os dois números de versão e por que existem dois, e as
verificações que condicionam um release — está em [docs/RELEASE.md](docs/RELEASE.md).

### Instalação no tablet

No tablet, como administrador:

```powershell
bcdedit /set testsigning on     # depois reiniciar uma vez
```

Copie `artifacts/driver-ln8000-arm64/` para o tablet, por exemplo para `C:\nabu-ln8000\`,
e execute o instalador a partir dessa pasta:

```powershell
cd C:\nabu-ln8000
.\install-driver.ps1           # verifica o modo de assinatura, instala, associa ACPI\QCOM057E, inicia
.\nabu-ln8000.ps1 status       # esperado: mode SWITCHING 2:1, ou BYPASS 1:1 como retorno seguro
```

Não é preciso alterar o ACPI nem regravar a UEFI: o nó `PEIC` (`_HID = QCOM057E`, I²C 0x51
em `\_SB.I2C5`) já está descrito na DSDT do tablet e o driver se associa a ele.

### Atualização e reversão

```powershell
.\update-driver.ps1                 # instala por cima, mantém o pacote anterior
.\uninstall-driver.ps1              # para o serviço e remove o pacote
pnputil /add-driver $env:ProgramData\nabu-fastcharge\backup\ln8000_kmdf.inf /install
```

O driver não escreve nada na firmware nem altera configurações de energia, então removê-lo
devolve o dispositivo ao comportamento anterior à instalação.

O resto do conjunto de ferramentas — `nabu-ln8000.ps1 status|sessions|read|write|journal`,
`run-acceptance.ps1` (o protocolo de aceitação automatizado) — está descrito em
[docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md). Um script PowerShell que contém texto não
ASCII é salvo em UTF-8 com BOM, porque o PowerShell 5.1, caso contrário, o decodifica como
ANSI e quebra a análise das aspas.

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
