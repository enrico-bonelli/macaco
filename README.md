# macaco

**Editor di testo per Linux leggero, veloce e completo**, scritto in Rust con
GTK 4.

macaco unisce la reattività di un editor minimale agli strumenti che servono
ogni giorno a chi lavora su progetti:
- evidenziazione della sintassi per 15 linguaggi;
- formattazione del codice;
- confronto tra file;
- pannello dei file, ricerca veloce dei file e ricerca di testo nel progetto;
- integrazione con git;
- terminale integrato.

Tutto questo senza rinunciare alla leggerezza: all'avvio occupa circa
**65 MB** di memoria, il binario pesa **2,7 MB** e le funzioni più pesanti
(terminale, indice dei file) si caricano solo quando le usi.

---

## Indice

- [Punti di forza](#punti-di-forza)
- [Funzionalità](#funzionalità)
  - [Scrittura e modifica](#scrittura-e-modifica)
  - [Linguaggi supportati](#linguaggi-supportati)
  - [Formattazione del codice](#formattazione-del-codice)
  - [Cerca e sostituisci](#cerca-e-sostituisci)
  - [Codifiche e fine riga](#codifiche-e-fine-riga)
  - [Schede, vista affiancata e confronto](#schede-vista-affiancata-e-confronto)
  - [Sessione e salvataggio automatico](#sessione-e-salvataggio-automatico)
  - [Cartella di lavoro e pannello file](#cartella-di-lavoro-e-pannello-file)
  - [Ricerca veloce dei file](#ricerca-veloce-dei-file)
  - [Cerca nei file](#cerca-nei-file)
  - [Git](#git)
  - [Terminale integrato](#terminale-integrato)
  - [Aspetto e preferenze](#aspetto-e-preferenze)
- [Scorciatoie da tastiera](#scorciatoie-da-tastiera)
- [Prestazioni e uso della memoria](#prestazioni-e-uso-della-memoria)
- [Dati salvati](#dati-salvati)
- [Requisiti](#requisiti)
- [Compilazione e installazione](#compilazione-e-installazione)
- [Sviluppo](#sviluppo)

---

## Punti di forza

- **Leggero:**
  - renderer software (`cairo`) invece di GPU, quindi niente driver
    GL/Vulkan in memoria;
  - liste virtuali per migliaia di file;
  - schede inattive scaricate dalla memoria;
  - memoria restituita al sistema quando chiudi file e cartelle.
- **Reattivo:**
  - colorazione della sintassi incrementale, in piccoli blocchi quando
    l'editor è inattivo;
  - git, scansioni e ricerche girano in background senza bloccare
    l'interfaccia.
- **Senza dipendenze pesanti:**
  - lexer scritti da zero e formattatore interno di riserva;
  - gli strumenti esterni (formattatori, `git`, VTE per il terminale) sono
    facoltativi e vengono usati solo se presenti.
- **Non perde il lavoro:** il contenuto non salvato finisce in una cache su
  disco poco dopo ogni modifica, e alla riapertura tutte le schede vengono
  ripristinate.
- **Pensato per i progetti:** cartella di lavoro, albero dei file, ricerca
  veloce, ricerca nei file, git e terminale nella stessa finestra.
- **Interfaccia in italiano.**

---

## Funzionalità

### Scrittura e modifica

- **Numeri di riga** nel margine ed **evidenziazione della riga corrente**
  (entrambi disattivabili).
- **Indentazione automatica:**
  - Invio mantiene il rientro e lo aumenta dopo `{`, `(`, `[` o `:`
    (Python, YAML);
  - la graffa di chiusura torna al livello giusto;
  - Backspace su un rientro di soli spazi cancella un livello intero.
- **Tab** e **Maiusc+Tab** aumentano e riducono il rientro delle righe
  selezionate; ci sono anche `Ctrl+]` e `Ctrl+[`.
- **Commenta/decommenta** le righe selezionate (`Ctrl+/`), con il commento
  giusto per ogni linguaggio (`//`, `#`, `--`, `<!-- -->`, `/* */`).
- **Duplica** (`Ctrl+Maiusc+D`) ed **elimina** (`Ctrl+Maiusc+K`) la riga.
- **Evidenziazione della parentesi corrispondente.**
- **Rimuovi spazi finali** (menu Modifica).
- **A capo automatico** attivabile (`Alt+Z`).
- **Zoom** del testo con `Ctrl++`, `Ctrl+-`, `Ctrl+0` o `Ctrl`+rotella.
- **Markdown:**
  - Invio continua elenchi puntati e numerati, task (`- [ ]`) e citazioni;
  - su una voce vuota l'elenco termina.
- **Vai alla riga** (`Ctrl+L`, anche nel formato `riga:colonna`).
- **Apri trascinando** i file sulla finestra.
- **File recenti** nel menu File.

### Linguaggi supportati

Ogni linguaggio ha un lexer interno con colorazione di parole chiave, tipi,
costanti, funzioni, stringhe, numeri, commenti e direttive.

| Linguaggio | Riconosciuto da |
|---|---|
| C, C++ | `.c` `.h` / `.cpp` `.cc` `.hpp` `.ino` … |
| Rust | `.rs` |
| Go | `.go` |
| Java | `.java` |
| Python | `.py` `.pyw` `.pyi`, shebang `python` |
| JavaScript, TypeScript | `.js` `.mjs` `.jsx` `.json` / `.ts` `.tsx` |
| PHP (anche misto a HTML) | `.php` `.phtml`, contenuto `<?php` |
| HTML (con CSS e JS incorporati), XML, SVG | `.html` `.htm` `.xml` `.svg` `.vue`, `<!DOCTYPE html>` |
| CSS, SCSS, Less | `.css` `.scss` `.less` |
| Bash e shell | `.sh` `.bash` `.zsh`, `PKGBUILD`, `.bashrc`, shebang `sh`/`bash`/`zsh` |
| YAML | `.yml` `.yaml`, `---` iniziale |
| Markdown | `.md` `.markdown` `.mdx` … |
| SQL (PostgreSQL, MySQL/MariaDB, SQLite) | `.sql` `.pgsql` `.psql` `.mysql` `.ddl` `.dml` |

- **Markdown** colora titoli, grassetto/corsivo/barrato, codice, link,
  immagini, elenchi, task, citazioni e HTML. I blocchi ```` ```lang ```` sono
  colorati con il linguaggio indicato (`rust`, `sql`, `bash`, `json`…).
- **SQL** riconosce parole chiave, tipi e funzioni senza distinguere
  maiuscole e minuscole, i commenti `--` e `/* */` e le stringhe su più
  righe.
- Il linguaggio si può sempre cambiare a mano, dalla barra di stato o dal
  menu *Linguaggio*.

### Formattazione del codice

**Formatta documento** (`Ctrl+Maiusc+I`) usa il formattatore standard del
linguaggio, se è installato:

| Linguaggio | Strumento |
|---|---|
| Rust | `rustfmt` |
| C, C++, Java | `clang-format` (rispetta un eventuale `.clang-format` del progetto) |
| JavaScript, TypeScript | `prettier`, poi `clang-format` |
| CSS, HTML, YAML, Markdown | `prettier` |
| Go | `gofmt` |
| Python | `ruff`, poi `black` |
| Bash | `shfmt` |
| SQL | `pg_format` |

Se nessuno è disponibile interviene un **formattatore interno**:
- ri-indenta in base a graffe, parole chiave della shell e tag HTML;
- per il Markdown pulisce il testo conservando gli a capo forzati e i
  blocchi di codice.

La formattazione è un'unica operazione annullabile.

### Cerca e sostituisci

- **Cerca** (`Ctrl+F`) e **Sostituisci** (`Ctrl+H`) in una barra in fondo
  alla finestra.
- Opzioni: maiuscole/minuscole, parola intera, **espressioni regolari**
  (con `$1`, `$2`… nella sostituzione).
- Tutte le occorrenze vengono evidenziate mentre scrivi, con il loro numero.
- `F3` / `Maiusc+F3` passano all'occorrenza successiva o precedente.
- **Sostituisci tutto** è un unico passo di annulla.

### Codifiche e fine riga

- Codifiche supportate, con rilevamento automatico: **UTF-8** (con o senza
  BOM), **UTF-16** LE/BE, ASCII, **ISO-8859-1**, ISO-8859-15,
  **Windows-1252**.
- Dalla barra di stato o dal menu *Codifica* puoi:
  - **convertire** il file in un'altra codifica: si applica al salvataggio,
    con un avviso se alcuni caratteri non sono rappresentabili;
  - **riaprirlo** interpretandolo con un'altra codifica.
- Il fine riga **LF/CRLF** viene rilevato, mantenuto al salvataggio e si può
  cambiare.

### Schede, vista affiancata e confronto

- **Schede:**
  - mostrano il nome del file o, per i documenti mai salvati, l'inizio del
    contenuto;
  - `•` indica modifiche non salvate;
  - si riordinano e si trascinano tra i pannelli;
  - si chiudono anche con il clic centrale.
- **Vista affiancata** (`Ctrl+\` o `F8`): due pannelli con le proprie
  schede; `Ctrl+Maiusc+M` sposta la scheda nell'altro pannello.
- **Confronto (diff)** tra le due schede affiancate (`F9`):
  - righe aggiunte in verde, rimosse in rosso, modificate in blu, con
    evidenziate le parole cambiate;
  - **scorrimento sincronizzato** dei due pannelli;
  - `Alt+↓` / `Alt+↑` saltano alla differenza successiva o precedente;
  - si ricalcola mentre scrivi, con un riepilogo nella barra di stato.

### Sessione e salvataggio automatico

- **Nessuna domanda all'uscita:**
  - alla riapertura tornano le schede aperte (salvate e non), la loro
    disposizione e la cartella di lavoro;
  - il contenuto non salvato viene scritto nella cache poco dopo ogni
    modifica, quindi sopravvive anche a un crash.
- **Schede inattive scaricate:** le schede nascoste da più di 30 secondi
  vengono tolte dalla memoria e ricaricate quando tornano visibili; all'avvio
  vengono caricate solo quelle visibili.
- **Istanza singola:** `macaco file…` apre i file come schede nella finestra
  già aperta.

### Cartella di lavoro e pannello file

**Cartella di lavoro.** Si apre con File → *Apri cartella di lavoro…*
(`Ctrl+Maiusc+O`) oppure con `macaco <cartella>`.
- È il modo di lavorare su un progetto: il pannello file resta su quella
  cartella ed è la base di ricerca veloce, ricerca nei file e git.
- Viene ricordata tra le sessioni.

**Pannello file** (`Ctrl+B` o `F7`):
- **Albero della cartella**, che si aggiorna da solo quando i file cambiano
  su disco. Le sottocartelle vengono lette solo quando le espandi.
- Senza cartella di lavoro mostra la cartella del file attivo; il file
  attivo viene sempre selezionato ed espanso.
- Doppio clic o Invio aprono un file; `↑` sale alla cartella superiore.
- **Menu contestuale** (clic destro):
  - nuovo file, nuova cartella;
  - rinomina (`F2`), sposta nel cestino (`Canc`);
  - copia percorso, apri terminale qui;
  - usa come cartella di lavoro;
  - le azioni git adatte al file.
- Rinominando file o cartelle aperti, le schede seguono il nuovo percorso.
- File nascosti e di backup (`~`) si mostrano dal menu `☰`.
- I file modificati secondo git sono contrassegnati da una lettera colorata
  (vedi [Git](#git)).

**Chiudi cartella di lavoro:**
- chiude tutti i documenti, chiedendo di salvare quelli modificati;
- *Annulla* interrompe e lascia tutto aperto;
- poi chiude la cartella, il pannello e le finestre collegate, e libera la
  memoria (albero, indice, monitor).

### Ricerca veloce dei file

**`Ctrl+P`** apre una finestra in cui scrivi parte del nome di un file.
- I risultati compaiono mentre digiti, con corrispondenza "fuzzy": `wgit`
  trova `src/window/gitui.rs`. Hanno la precedenza i risultati nel nome del
  file e le corrispondenze consecutive.
- I caratteri trovati sono in grassetto.
- Su/Giù per scegliere, Invio o clic per aprire, Esc per chiudere.

**Indicizzazione:**
- La ricerca vale per le cartelle di lavoro che decidi di **indicizzare**
  (File → *Indicizza la cartella di lavoro*, oppure "Indicizza ora" al
  primo `Ctrl+P`).
- L'elenco dei file viene salvato su disco, caricato all'apertura della
  cartella e aggiornato in background quando apri la ricerca. I file creati
  o cancellati compaiono e scompaiono da soli; il disco non viene sorvegliato
  di continuo.
- Sono **esclusi** i file dentro cartelle nascoste (`.git`, `.cache`,
  `.github`…) e i file ignorati da git, anche nelle sottocartelle che sono
  repository.

**Peso misurato:**

| Progetto | Memoria dell'indice | Tempo per tasto |
|---|---|---|
| 21.000 file | ~2 MB | pochi ms |
| 450.000 file | ~24 MB | pochi ms |

### Cerca nei file

**`Ctrl+Maiusc+F`** cerca una parola o una frase in tutti i file della
cartella di lavoro.
- **Avvio:** opzioni maiuscole/minuscole, parola intera, espressione
  regolare. La ricerca parte con il pulsante **Ricerca** (o Invio) e si può
  **interrompere**; la riga di stato mostra l'avanzamento.
- **Risultati:** una riga per ogni riga di file trovata, con nome del file,
  numero di riga e testo, la parte trovata in grassetto.
- **Anteprima:** un clic mostra il file nel punto trovato, con numeri di
  riga, sintassi colorata e tutte le occorrenze evidenziate.
- **Apri** (su ogni risultato o sopra l'anteprima) o il doppio clic aprono il
  file in una scheda alla riga trovata. Puoi aprirne più di uno: la finestra
  resta aperta.
- **File cercati:** gli stessi della ricerca veloce, senza file binari né
  file oltre 8 MB, fino a 10.000 righe. Non serve l'indicizzazione.
- **Chiusura:** la finestra si chiude a mano, e chiudendola risultati e
  anteprima vengono liberati dalla memoria.

### Git

macaco usa il comando `git` installato. Il repository è quello della
cartella di lavoro o, se non c'è, del file attivo.

**Stato visibile:**
- Nel pannello file, lettera e colore per ogni file:

  | Segno | Significato |
  |---|---|
  | `M` (giallo) | file modificato |
  | lettera in blu (`A`, `M`, `R`…) | modifica già nell'area di stage |
  | `U` (verde) | file non tracciato |
  | `!` (rosso) | file in conflitto |
  | `•` | cartella che contiene modifiche |

- Nella barra di stato, il **ramo corrente** con i commit da inviare e da
  ricevere (`⎇ main ↑1 ↓2`). Diventa rosso in caso di conflitti; il clic apre
  la finestra dei rami.
- Lo stato si aggiorna da solo: al salvataggio, dopo ogni comando, quando la
  finestra torna attiva e quando cambia la cartella `.git` (anche per comandi
  dati nel terminale).

**Finestra "Modifiche"** (`Ctrl+Alt+G`, o clic destro su una cartella):
- Elenca i file modificati o non tracciati della cartella, ognuno con una
  casella di selezione.
- Comandi: **add**, **commit dei soli file selezionati**, **commit e push**,
  **pull**, **push**.
- **Filtro:** mostra solo i file che corrispondono, per esempio `.pdf`,
  `src/`, `*.log`, oppure più termini.
- **Esclusioni:**
  - *Nascondi selezionati*, o il menu `⋮` della riga, escludono file o
    cartelle dall'elenco;
  - l'esclusione è ricordata per quel repository e non cambia nulla per git;
  - con *Mostra esclusi* puoi recuperare i file esclusi.

  Esempio, togliere tutti i PDF: *Nessuno* → filtro `.pdf` →
  *Seleziona tutto* → *Nascondi selezionati*.
- Il contatore indica quanti file finiranno nel commit, anche quelli nascosti
  dal filtro.

**Menu Git e menu contestuale:**
- **Su file e cartelle:** "aggiungi al repository" per i file nuovi, "metti
  in stage le modifiche" per i file modificati, rimuovi dallo stage, annulla
  le modifiche (con conferma), segna il conflitto come risolto.
- **Commit** (`Ctrl+Alt+C`), con elenco dei file, messaggio, opzioni `-a` e
  `--amend`.
- **Sincronizzazione:** pull, push e fetch. Al primo push il ramo viene
  creato sul remoto con `-u`.
- **Rami e merge:** passa a un ramo locale o remoto, crea un ramo, unisci un
  ramo nel corrente, interrompi il merge.
- `git init` per una cartella nuova.

**Comportamento:**
- I documenti aperti vengono salvati prima di add e commit, e riletti dopo
  pull, merge, cambio di ramo e annullamento delle modifiche. Quelli con
  modifiche non salvate non vengono mai toccati.
- Git non chiede mai password, quindi l'editor non resta bloccato. Per
  push e pull servono ssh-agent o un credential helper; se mancano le
  credenziali, il messaggio suggerisce di usare il terminale integrato.

### Terminale integrato

**`F12`** (o ``Ctrl+` ``) apre un pannello in basso con la tua shell
(`$SHELL`), nella cartella del file attivo.
- **Sessione:** nascondere il pannello non chiude la shell; `exit` lo chiude.
- **Cambio cartella:** `Maiusc+F12` porta la shell nella cartella del file
  corrente, se non sta eseguendo un comando. "Apri terminale qui" nel
  pannello file fa lo stesso con una cartella qualsiasi.
- **Tastiera:** mentre il terminale ha il focus, i tasti vanno alla shell
  (`Ctrl+C`, `Ctrl+R`, `Ctrl+W`, tasti funzione…); restano attivi solo
  `F12`, il cambio scheda e lo zoom. Copia e incolla con `Ctrl+Maiusc+C/V`.
- **Aspetto:** stessi colori e carattere dell'editor. L'avviso sonoro (per
  esempio sul completamento con Tab) si disattiva nelle Preferenze.
- **Requisito:** serve la libreria VTE per GTK 4 (pacchetto `vte4`). Viene
  caricata solo alla prima apertura del pannello, quindi se non usi il
  terminale non occupa memoria.

### Aspetto e preferenze

**Preferenze** (`Ctrl+,`):
- carattere (con filtro sui caratteri a spaziatura fissa) e dimensione;
- larghezza del tab, spazi al posto dei tab;
- a capo automatico, numeri di riga, evidenziazione della riga corrente;
- tema chiaro/scuro (al primo avvio segue quello di sistema);
- avviso sonoro;
- cursore lampeggiante (disattivato per risparmiare CPU).

**Barra di stato:**
- posizione del cursore (clic: vai alla riga), linguaggio, codifica e fine
  riga (cliccabili);
- ramo git e riepilogo del confronto;
- memoria in uso dal processo, aggiornata ogni 2 secondi.

---

## Scorciatoie da tastiera

| Tasto | Azione |
|---|---|
| `Ctrl+N` / `Ctrl+T` | nuovo documento |
| `Ctrl+O` | apri file |
| `Ctrl+S` / `Ctrl+Maiusc+S` | salva / salva con nome |
| `Ctrl+W` / `Ctrl+Q` | chiudi scheda / esci |
| `Ctrl+Tab`, `Ctrl+PagGiù` / `Ctrl+Maiusc+Tab`, `Ctrl+PagSu` | scheda successiva / precedente |
| `Ctrl+Z` / `Ctrl+Maiusc+Z`, `Ctrl+Y` | annulla / ripeti |
| `Ctrl+F` / `Ctrl+H` | cerca / sostituisci |
| `F3`, `Ctrl+G` / `Maiusc+F3` | occorrenza successiva / precedente |
| `Ctrl+L` | vai alla riga |
| `Ctrl+/` | commenta / decommenta |
| `Ctrl+]` / `Ctrl+[` | aumenta / riduci rientro |
| `Ctrl+Maiusc+D` / `Ctrl+Maiusc+K` | duplica / elimina riga |
| `Ctrl+Maiusc+I` | formatta documento |
| `Alt+Z` | a capo automatico |
| `Ctrl++` / `Ctrl+-` / `Ctrl+0` | zoom |
| `Ctrl+\`, `F8` | affianca schede |
| `Ctrl+Maiusc+M` | sposta scheda nell'altro pannello |
| `F9` | confronta le schede affiancate |
| `Alt+↓` / `Alt+↑` | differenza successiva / precedente |
| `Ctrl+Maiusc+O` | apri cartella di lavoro |
| `Ctrl+B`, `F7` | pannello file |
| `Ctrl+P` | ricerca veloce dei file |
| `Ctrl+Maiusc+F` | cerca nei file della cartella di lavoro |
| `Ctrl+Alt+G` | modifiche git della cartella |
| `Ctrl+Alt+C` | commit |
| `F12`, ``Ctrl+` `` | terminale |
| `Maiusc+F12` | terminale nella cartella del file |
| `F2` / `Canc` (nel pannello file) | rinomina / sposta nel cestino |
| `Ctrl+,` | preferenze |

---

## Prestazioni e uso della memoria

**Avvio leggero:**
- circa 65 MB di RSS all'avvio;
- renderer GTK `cairo` predefinito, invece dei circa 350 MB con GL/Vulkan.
  Si può cambiare con `renderer=gl|vulkan|auto` in `settings.conf` o con la
  variabile `GSK_RENDERER`.
- Binario di 2,7 MB (LTO, strip).

**Colorazione della sintassi:**
- incrementale: per ogni riga si memorizza lo stato del lexer, e dopo una
  modifica si rianalizzano solo le righe necessarie, a blocchi di pochi
  millisecondi;
- un'unica tabella di stili condivisa tra tutti i documenti.

**Memoria restituita al sistema:**
- chiudendo un documento, la cartella di lavoro, il terminale o le finestre
  di ricerca, la memoria liberata torna al sistema (`malloc_trim`). Dopo aver
  chiuso un file da 15 MB l'uso di memoria torna al valore di partenza.
- Un'unica arena dell'allocatore: la memoria dei lavori in background non
  resta trattenuta dai thread che l'hanno usata.

**Interfaccia:**
- liste virtuali (pannello file, modifiche git, ricerca veloce, risultati
  della ricerca): esistono solo le righe visibili, anche con migliaia di
  elementi;
- cursore fermo per impostazione predefinita: in GTK 4 il lampeggio ridisegna
  la vista a ogni fotogramma (circa 15% di un core per 10 s dopo ogni tasto).

**Tempi misurati:**

| Operazione | Tempo |
|---|---|
| Apertura di un file da 15 MB (200.000 righe) | 0,3 s |
| Digitazione nello stesso file | sotto i 5 ms per tasto |
| Lettura dello stato git su 20.000 file | circa 50 ms, in background |
| Ricerca nei file su 4.400 file | 0,2 s |

---

## Dati salvati

Tutto si trova in `~/.macaco/`:

| Percorso | Contenuto |
|---|---|
| `config/settings.conf` | preferenze (testo `chiave=valore`) |
| `session` | schede aperte e cartella di lavoro |
| `cache/<id>.txt`, `.meta` | contenuto delle schede non salvate |
| `recent` | file recenti |
| `indexed` | cartelle indicizzate per la ricerca veloce |
| `index/<id>.paths` | elenco dei file di ogni cartella indicizzata |
| `git-excluded` | file esclusi dall'elenco delle modifiche git, per repository |

---

## Requisiti

**Per eseguire macaco:**
- Linux con GTK ≥ 4.12 (pacchetto `gtk4`).

**Facoltativi, per le funzioni corrispondenti:**

| Pacchetto (Arch) | Funzione |
|---|---|
| `vte4` | terminale integrato |
| `git` | integrazione git |
| `rust` (rustfmt), `clang`, `go`, `ruff` / `python-black`, `shfmt`, `prettier`, `pgformatter` | formattazione dei rispettivi linguaggi |

**Per compilare:**
- Rust ≥ 1.88 (edizione 2024);
- `cargo`;
- file di sviluppo di GTK 4.

Le librerie Rust usate sono:
- `gtk4` (con `pangocairo`) per l'interfaccia;
- `regex` per la ricerca;
- `similar` per il confronto;
- `libc` per le chiamate di sistema.

---

## Compilazione e installazione

```sh
git clone https://github.com/enrico-bonelli/macaco.git
cd macaco
cargo build --release
```

Il binario si trova in `target/release/macaco`. Per installarlo nella propria
home, con la voce nel menu delle applicazioni e l'icona:

```sh
install -Dm755 target/release/macaco ~/.local/bin/macaco
install -Dm644 data/io.github.macaco.Editor.desktop -t ~/.local/share/applications/
install -Dm644 data/io.github.macaco.Editor.svg -t ~/.local/share/icons/hicolor/scalable/apps/
install -Dm644 data/io.github.macaco.Editor.metainfo.xml -t ~/.local/share/metainfo/
```

Per un'installazione di sistema si usano gli stessi file con destinazione
`/usr/bin`, `/usr/share/applications`, `/usr/share/icons/hicolor/scalable/apps`
e `/usr/share/metainfo`.

**Uso:**
- `macaco [file…]` apre i file;
- `macaco <cartella>` la apre come cartella di lavoro.

---

## Sviluppo

```sh
cargo test                     # lexer, codifiche, formattazione, ricerca, git, indice
cargo build --profile perf     # ottimizzato, con gli strumenti di debug attivi
```

**Struttura dei sorgenti** (`src/`):

| File | Contenuto |
|---|---|
| `main.rs` | avvio, istanza singola, apertura dei file |
| `window.rs` | finestra principale: menu, schede, ricerca, preferenze, sessione, pannelli |
| `window/gitui.rs` | interfaccia git: stato, comandi, finestre di commit, rami e modifiche |
| `window/quickopen.rs` | ricerca veloce dei file (`Ctrl+P`) |
| `window/textsearch.rs` | ricerca nei file con anteprima |
| `doc.rs` | documento: buffer, colorazione incrementale, margine, indentazione |
| `lexer.rs`, `lang.rs` | lexer e definizioni dei linguaggi |
| `format.rs` | formattatori esterni e interni, commenti |
| `search.rs`, `diff.rs` | ricerca con espressioni regolari, confronto tra testi |
| `encoding.rs` | rilevamento e conversione delle codifiche |
| `sidebar.rs` | pannello file |
| `git.rs` | esecuzione di git e lettura dello stato |
| `finder.rs` | elenco dei file, corrispondenza fuzzy, indice, ricerca nei file |
| `terminal.rs` | terminale (VTE caricata dinamicamente) |
| `config.rs` | preferenze, sessione, cache, file in `~/.macaco/` |

**Script di prova:** nelle build di debug la variabile `MACACO_DEBUG` esegue
una sequenza di azioni senza interazione, ad esempio:

```sh
MACACO_DEBUG="open:/tmp/a.rs,split,format,shot:/tmp/s.png" target/debug/macaco
```

Sono disponibili anche i passi `rss` (memoria e documenti vivi), `dump`,
`type:`, `key:`, `find:` e altri. I passi che impiegano più di 5 ms vengono
segnalati.
