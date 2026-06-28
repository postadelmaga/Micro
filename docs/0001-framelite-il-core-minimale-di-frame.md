# ADR 0001 — framelite: il core minimale e generico di Frame

- **Stato**: Accettato
- **Data**: 2026-06-28
- **Decisori**: Fra
- **Relazione con Frame**: framelite **estrae e generalizza** il cuore architetturale di Frame
  (ADR-0003 "architettura ibrida a moduli: core in-process + bus astratto"), scartando tutto il
  peso di dominio. Non è un fork di Frame: è la base su cui un domani Frame (o un'altra app) può
  poggiare.

## 1. Contesto

Frame è un framework completo per app desktop: UI dioxus-native/Vello, renderer 3D rend3 come
sidecar, synth `cpal`, plugin sandboxati, scene/kernel/alberi procedurali. Il **valore
architetturale** però non sta in quei moduli, sta nello scheletro che li tiene insieme:
**moduli che parlano solo via un bus astratto, instradati per canale, su uno stato a documento
undoable**. Vogliamo quello scheletro da solo — minimale, generico, riusabile — su cui basare
nuove app senza trascinarsi 3D, audio e plugin.

## 2. Decisione

Creare `framelite` come **workspace sorella** di `Frame`, che implementa SOLO il core e lo rende
**generico**:

1. **Canali a stringa** invece dell'enum fisso di Frame. In Frame `ChannelId` è
   `Audio|Scene|Control|Input` (dominio ZenFlow); in framelite un canale è un `Channel(String)`,
   così un'app dichiara i propri topic senza toccare il crate `protocol`.
2. **Un solo transport: `LocalBus` in-process.** Frame ha Local + Ipc + ring shmem per i sidecar;
   framelite tiene solo il broker pub/sub in-memory. I sidecar/rete restano una *cucitura* (i
   moduli scrivono contro i trait `Sender`/`Receiver`), non codice da spedire ora.
3. **`Doc<S>` generico undoable**, senza migrazioni né schemi di dominio. Ogni edit è un
   `Command<S>` applicato in modo transazionale (su un clone; lo stato vivo cambia solo se il
   comando ha successo).
4. **Niente dominio.** Nessun renderer, audio, scene, plugin. L'app che usa framelite li aggiunge.

## 3. Architettura

```
framelite-protocol   ModuleId · Channel(String) · Envelope · ChannelKind     (zero logica)
        ▲
framelite-bus        trait Sender/Receiver + LocalBus (pub/sub, retained channels)
framelite-document   History<S> · Command<S> · Doc<S>                          (undo/redo)
        ▲                        ▲
framelite-core       Module · ModuleCtx · Runtime          (micro-kernel in-process)
```

**Confini (invariante):** `protocol` non dipende da nulla; `bus` dipende solo da `protocol`;
`document` non dipende da nessuno dei due; solo `core` compone tutto. Un modulo comunica **solo**
via `ModuleCtx` (publish + recv) — mai con un altro modulo direttamente.

### Retained channels = la versione generica del "replay State" di Frame

Frame distingue canali `State` (rigiocabili: la scena) da `Event` (no: una nota, uno shutdown).
In framelite la distinzione non è cablata nel nome del canale: l'app marca i canali stateful con
`bus.retain("count")`. Un canale retained conserva l'ultimo `Envelope` e lo consegna a ogni nuovo
subscriber — un modulo che si aggancia tardi impara subito il valore corrente.

## 4. Conseguenze

**Positive:** scheletro riusabile, generico e leggero; "scrivi il modulo una volta" preservato
dai trait del bus; confini machine-enforced dal grafo dei crate; un esempio (`counter`) prova
end-to-end bus+document+moduli in ~20 righe per modulo.

**Costi:** framelite **non** è eseguibile come app (niente UI/host) — è una libreria-fondamenta;
le feature pesanti (sidecar, shmem, plugin, UI) sono deliberatamente assenti e vanno aggiunte
sopra, esattamente lungo le cuciture che Frame ha già esplorato.

## 5. Alternative considerate

- **Estrarre i crate di Frame così com'erano** → respinta: `protocol` era legato al dominio
  (canali Audio/Scene), `bus` portava shmem/postcard/libc. Generalizzare era il punto.
- **Un unico crate monolitico** → respinta: i confini fra crate *sono* l'architettura; tenerli
  separati è ciò che impedisce a `document`/`bus` di dipendere dai moduli.
