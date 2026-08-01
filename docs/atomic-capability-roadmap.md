# Feuille de route de parité Atomic → Orkia

> État de l'audit : 1 août 2026. Ce document est une trace de décision et de
> livraison, pas une déclaration de parité. Toute ligne marquée *vérifiée* est
> reliée à un élément de code inspecté ; toute autre ligne reste un objectif.

## Objet et méthode

Orkia est un **moteur sémantique Git-native** : Git est la source de contenu,
de synchronisation et de branches ; Orkia ajoute la capture causale signée,
la politique et la projection de review. Atomic est un système de contrôle de
version autonome, doté d'un moteur CRDT. La parité recherchée est donc une
parité de capacité observable pour le flux capture → compréhension → review
→ intégration, et non le remplacement du stockage Git par le CRDT d'Atomic.

La source auditée est
`/Volumes/MyWorld/monkey.d.labs/coding/riftrHQ/_killix/inspiration_code/atomic`.
Les preuves sont les commandes déclarées dans
`atomic-cli/src/main.rs`, les crates du workspace dans `Cargo.toml`, et leurs
modules publics. L'état Orkia provient du code de ce dépôt et du test
`cargo test --workspace -q`, exécuté avec succès lors de l'audit (17 tests).

Légende :

| Statut | Sens |
| --- | --- |
| Vérifié | Implémenté et couvert par une vérification locale ciblée. |
| Partiel | Du code existe, mais il ne couvre pas la capacité définie ou n'est pas assemblé. |
| Absent | Aucune implémentation correspondante trouvée. |
| Décision | Capacité Atomic qui n'a pas d'équivalent Git-native direct ; une décision produit est requise. |

## Verdict actuel

La parité avec Atomic n'est **pas** actuellement respectée. Orkia possède les
fondations du flux Git-native (ledger signé, capture, extraction d'atomes,
plan de review, politique et projection GitHub), mais pas encore la profondeur
fonctionnelle d'Atomic. La capture des agents conserve désormais les sources
brutes **et** normalise des actions typées pour les huit adaptateurs Orkia.
Pour les hooks live, une session externe est liée durablement à une session
Orkia et reçoit des snapshots Git au démarrage puis aux checkpoints/fins. Les
imports historiques restent volontairement non liés tant qu'ils ne fournissent
pas un état Git de départ vérifiable.

Les limites suivantes sont établies par le code actuel :

- `orkia-agents/src/lib.rs` découvre les transcripts, installe les hooks et
  produit des `AgentAction` typées (prompt, outil, lecture, écriture, commande
  et tour/token) pour Claude Code, Codex, Gemini, Kimi, OpenCode, Cursor,
  Droid et Qwen. Les sources non modifiées restent dans le ledger. Les hooks
  live lient une session externe, ses actions et ses snapshots Git ; les
  imports historiques réutilisent une liaison live connue ou restent
  fail-closed pour la review. Un document identique n'est pas réimporté.
- `orkia-semantic/src/lib.rs` reconnaît neuf grammaires, mais ses dépendances
  actuelles sont surtout des heuristiques de même fichier et de tests ; ce
  n'est pas encore un graphe de références sémantiques.
- `orkia-git/src/lib.rs` projette des fichiers complets, pas des plages ou
  atomes intra-commit. `orkia-review/src/lib.rs` contient un plan fermé et
  fail-closed, mais ses identifiants d'atomes sont recréés à l'extraction.
- `orkia-server/src/main.rs` expose santé, authentification minimale et
  réception de webhook ; il n'assemble ni registre de dépôts, ni index
  Postgres, ni orchestration GitHub App. `orkia-github/src/lib.rs` crée une
  pull request avec un jeton déjà fourni, sans cycle complet GitHub App.

## Inventaire exhaustif du plan de commande Atomic

Les 37 variantes de `enum Commands` dans `atomic-cli/src/main.rs` constituent
la surface CLI publiée auditée. Elles sont listées ici pour éviter de perdre
des capacités dans le découpage de la feuille de route.

| Domaine | Commandes Atomic | Capacité observable |
| --- | --- | --- |
| Agent et provenance | `agent`, `intent`, `memory`, `provenance` | Capture d'agents, intention, mémoire, provenance W3C PROV. |
| Cycle de travail | `init`, `status`, `record`, `revise`, `log`, `diff`, `doctor` | Création, état, enregistrement, révision, historique, diagnostic. |
| Contenu et changements | `add`, `remove`, `move`, `restore`, `change`, `insert`, `unrecord` | Modifications adressables, insertion, restauration et annulation d'enregistrement. |
| Isolation et vues | `sandbox`, `split`, `view`, `stash`, `tag` | Espaces isolés, séparation, vues, stash et tags. |
| Interop Git | `git`, `push`, `pull`, `clone` | Import/interop Git et synchronisation distante. |
| Identité et organisations | `identity`, `org`, `team` | Identité, organisations et équipes. |
| Réseau et service | `remote`, `server`, `workspace`, `project`, `update` | Remotes, profil serveur, workspaces, projets et mise à jour. |
| Recherche et connaissances | `query`, `vault` | Recherche contenu/graphe/entités et coffre de connaissances. |

## Capacités Atomic par sous-système

| Sous-système Atomic et preuves | Capacités relevées | Équivalent Orkia actuel | État |
| --- | --- | --- | --- |
| `atomic-core/src/crdt/trunk.rs` | CRDT de tronc avec états vivant, supprimé et zombie ; application, diff, merge, pristine, record. | Git conserve le contenu ; `orkia-git` compare et projette. Aucun CRDT Orkia. | Décision |
| `atomic-repository/src/changestore/mod.rs` et modules `repository/` | Change store adressé par hash, cache, archive, OCI, redb, historique, ignore, insertion, matérialisation, sandbox, tags, tracking, vues. | Ledger signé dans `refs/orkia/ledger`, diff Git et worktree isolé déclaré. Pas de magasin de changements, archive, vue ni tracking équivalents. | Partiel |
| `atomic-identity` (`delegation`, `keypair`, `signing`, `store`, `usage`) | Identités, clés, signatures, délégation et usage des clés. | `orkia-identity` signe et vérifie en Ed25519 ; aucune délégation ni cycle complet de clés inspecté. | Partiel |
| `atomic-agent/src/hooks/mod.rs` | Contrat `AgentHook`, détection/install/uninstall ; registre de 15 adaptateurs : Agy, Claude, Cline, Codex, Copilot, Cursor, Devin, Gemini CLI, Grok, Hermes, Kilo, Kiro, OpenCode, Pi, Sherpa. | `orkia-agents` gère huit agents (Claude Code, Codex, Gemini, Kimi, OpenCode, Cursor, Droid, Qwen) et l'installation de hooks. | Partiel |
| `atomic-agent` (`envelope`, `event`, `export`, `record`, `transcript`, `turn`, `watcher`, `provenance`, `learnings`) | Enveloppes/événements, tours, transcripts, exports, watcher, classification/consolidation de provenance et apprentissages. | Sources brutes plus actions typées pour les huit adaptateurs Orkia ; watcher défini dans `orkia-capture` mais non assemblé ; aucun mapping de session durable ni apprentissage. | Partiel |
| `atomic-semantic` | Parseurs C++, Go, Java, Python, Rust, Swift, TypeScript/JavaScript ; entités et références. | Les neuf langages sont reconnus dans `orkia-semantic` ; extraction d'atomes limitée et références absentes. | Partiel |
| `atomic-canonical` (`jcs`, `did`, `proof`, `prov`, `memory`, `gate`, `render`) | Canonicalisation JCS, DID, preuves, PROV, mémoire et gates. | Chaînage SHA-256 et signature Ed25519 dans `orkia-ledger`; sérialisation JSON ordinaire, sans JCS/DID/PROV ni gates canoniques. | Partiel |
| `atomic-remote` (`http`, `streaming`, `sync`, `storage`, `version`) | Upload/download, requêtes, protocole de streaming, négociation et synchronisation. | Git sync reste disponible au niveau Git ; aucune implémentation Orkia du protocole distant/sync de ledger. | Absent |
| `atomic-teams` (`org`, `team`, `member`, `grant`) | Organisations, équipes, membres, droits. | Aucune modélisation organisation/équipe/délégation équivalente. | Absent |
| CLI `query` et modules repository de recherche | Recherche de contenu, graphe, voisins, entités, code, enrichissement. | `orkia-index-postgres` contient une projection/recherche simple, non branchée au serveur et sans graphe sémantique. | Partiel |
| CLI `vault`, `intent`, `memory` | Coffre, intention, mémoire, compétences et contexte de session. | Objectif de session et prompts sont conservés ; pas de vault/mémoire/intentions versionnés. | Partiel |
| CLI `sandbox`, `workspace`, `project`, `server` | Espaces de travail, projets, isolations et profil serveur. | Création de worktree Git disponible dans l'adaptateur mais non exposée ni orchestrée ; serveur minimal. | Partiel |

## Cartographie précise des priorités Orkia

La colonne « responsable » désigne la crate qui doit porter la logique. Les
composition roots (`orkia-cli`, `orkia-server`) ne doivent pas absorber cette
logique.

| Priorité | Résultat à livrer | Responsable | Critère d'acceptation vérifiable |
| --- | --- | --- | --- |
| P0 | Fixer une matrice de compatibilité Atomic et des fixtures de comportement. | `tests/compat`, toutes crates | Chaque capacité de cet inventaire a un statut, une preuve, un test de non-régression ou une décision explicite. |
| P0 | Préserver le fail-closed : aucune stack automatique lorsque la capture est incomplète ou la confiance insuffisante. | `orkia-capture`, `orkia-review`, `orkia-policy` | Tests démontrant qu'une écriture inconnue, un transcript incomplet ou une dépendance non résolue produit une review unique. |
| P1 — en cours | Normaliser les transcripts et hooks en événements causaux typés : tour, prompt, appel/résultat d'outil, fichiers lus/écrits, commande, validation, coût et tokens. Les huit adaptateurs Orkia sont couverts ; il reste les fixtures anonymisées de corpus et la validation croisée de format. | `orkia-agents`, `orkia-capture`, `orkia-model` | Des fixtures réelles, anonymisées, deviennent les mêmes événements de domaine quel que soit le format fournisseur. Aucune unité automatique ne dépend d'un transcript brut. |
| P1 — en cours | Lier de manière stable la session externe de l'agent à la session Orkia, avec snapshot Git initial/final, hashes de contenu et provenance du hook. Les hooks live ont la liaison append-only, les checkpoints Git et le blocage de couverture en cas d'écriture inconnue. | `orkia-agents`, `orkia-git`, `orkia-ledger` | Relance, import différé et réception de hook ne créent pas de session dupliquée ; le ledger reconstruit la même relation. |
| P1 | Terminer le cycle hook des huit adaptateurs Riftr déjà présents, puis décider explicitement lesquels des quinze adaptateurs Atomic sont requis. | `orkia-agents` | Installation, désinstallation, idempotence, détection et capture sont testées sur macOS/Linux pour chaque agent retenu. Une capacité non retenue est documentée comme exclusion, non ignorée. |
| P1 | Brancher le watcher humain et `orkia run` aux mêmes événements typés que les agents. | `orkia-capture` | Toute écriture observée est reliée à une session ; une écriture inconnue dégrade la couverture de façon reproductible. |
| P2 | Rendre les atomes stables et adressables par contenu/plage/symbole, puis extraire les références/imports/tests/configurations/migrations réelles. | `orkia-semantic`, `orkia-model` | Deux extractions du même arbre produisent les mêmes IDs ; les dépendances de fixtures multi-fichiers correspondent aux références AST attendues. |
| P2 | Construire le graphe causal : arêtes dures (syntaxe/référence) et souples (capture), score expliqué et fermeture transitive des dépendances. | `orkia-review`, `orkia-semantic`, `orkia-capture` | Le plan est déterministe, fermé sur ses prérequis et explique chaque arête par une preuve versionnée. |
| P2 | Projeter des patches atomiques, non des fichiers complets, et représenter correctement une unité dépendant de plusieurs unités. | `orkia-git`, `orkia-forge`, `orkia-review` | Un même checkpoint produit plusieurs commits et branches sans perte de changement ; une dépendance multi-parent est reconstruite avec tous ses ancêtres. |
| P2 | Compléter la décision reviewer : approbation, demande de changements, fusion, scission et révision signée du plan. | `orkia-review`, `orkia-cli`, `orkia-ledger` | Une correction crée une nouvelle révision de plan sans altérer les événements causaux précédents ; le plan aval est reprojeté. |
| P3 | Rendre le ledger réellement canonique et synchronisable : JCS ou format canonique spécifié, objets append-only, rotation/délégation de clés et reconstruction depuis les refs. | `orkia-ledger`, `orkia-identity`, `orkia-git` | La même suite d'événements donne exactement le même hash sur macOS/Linux ; corruption, réordonnancement et signature non autorisée sont rejetés. |
| P3 | Exploiter réellement les worktrees concurrents et synchroniser le ledger via Git sans écrasement de producteurs. | `orkia-git`, `orkia-capture` | Deux sessions simultanées sont isolées, publiées puis reconstruites sans perte d'événement. |
| P3 | Assembler le serveur : registre de dépôts, politiques, index Postgres reconstructible, orchestration et reprise de jobs. | `orkia-server`, `orkia-index-postgres`, `orkia-ports` | La perte de Postgres est réparée intégralement depuis les refs ; l'API peut retrouver dépôt, plan et provenance. |
| P3 | Livrer le vrai adaptateur GitHub App : JWT/installations, branches, PRs empilées, checks, webhooks et protections. | `orkia-github`, `orkia-forge`, `orkia-server` | Un dépôt de test protégé refuse une intégration sans check/politique Orkia et accepte le chemin `orkia integrate` valide. |
| P4 | Porter ou décider les capacités Atomic hors cœur de review : query/graphe, vault/mémoire, équipes/grants, remotes, sandbox/vues/tags/stash, archive/OCI. | Crates dédiées à créer si la décision est « porter » | Chaque capacité reçoit « portée », « intégrée via Git », ou « hors périmètre v0.x », avec justification et test lorsque portée. |
| P4 | Mesurer le résultat sur Ghost PR et en bout-en-bout. | `tests/benchmark`, toutes crates | Gain ≥20 % sur le meilleur baseline, <10 % de paires corrigées séparées, ARI ≥0,8 après split/squash ; matrices Codex/Claude et humain macOS/Linux vertes. |

## Ordre d'exécution immédiat : capture des agents

Le premier incrément P1 a introduit un contrat de normalisation interne et des
tests synthétiques pour les huit adaptateurs. Le prochain consiste à figer des
fixtures anonymisées, issues de chaque format réel, puis à rattacher chaque
action à un état Git et à une session Orkia stable. Les installateurs de hooks
ne seront considérés comme parité que lorsque ces deux propriétés sont
vérifiées sur macOS et Linux.

Le contrat cible minimal est :

1. un identifiant externe stable, scoped par dépôt et fournisseur ;
2. une suite ordonnée de tours et d'actions typées ;
3. les fichiers lus, modifiés et leur état Git avant/après ;
4. les commandes, outils, résultats, erreurs, coût et tokens quand le
   fournisseur les expose ;
5. une provenance du format et du fichier source permettant une
   reconstruction/audit ;
6. une couverture calculée à partir de preuves observées, jamais supposée.

## Décisions d'architecture à ne pas masquer

Atomic peut fusionner au niveau de son CRDT ; Orkia ne doit pas tenter de
copier ce mécanisme par-dessus Git. Pour Orkia, l'équivalent utile est une
projection Git déterministe de patches atomiques, avec conflit explicite et
replanification si les préconditions changent. Les fonctions Atomic de
magasin, vue, sandbox ou synchronisation doivent donc être classées au cas par
cas : adaptation Git, nouveau service Orkia, ou exclusion assumée. Les appeler
« parité » avant cette décision serait trompeur.

La sortie v0.1 ne peut être déclarée qu'après les critères P0 à P4 applicables
au périmètre retenu. Le présent document doit être mis à jour dans la même
modification que chaque capacité : preuve source, test exécuté, statut et
éventuelle décision de périmètre.
