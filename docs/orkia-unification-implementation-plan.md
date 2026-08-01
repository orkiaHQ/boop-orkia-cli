# Plan d’implémentation — Orkia CLI, backend, frontend et Open-Core

> Statut : plan d’exécution détaillé.
>
> Ce document complète la feuille de route
> `orkia-unification-open-core-roadmap.md` et le plan
> `changeset-stack-execution-plan.md`. Il décrit l’ordre d’implémentation,
> les changements attendus, les tests et les critères de sortie. Il ne déclare
> aucune phase terminée avant que ses critères soient vérifiés dans le code et
> dans les scénarios E2E réels.

## 0. Principes non négociables

### 0.1 Parcours utilisateur nominal

Le parcours agent ne demande jamais à l’auteur de déclarer une session, une
intention, une stack ou un ChangeSet :

```text
orkia init
  ↓
agent lancé normalement dans le repository
  ↓
hooks SessionStart / prompts / outils / fichiers / validations
  ↓
session + intention + atomes automatiques
  ↓
StackPullRequest + Stack + ChangeSet automatiques
  ↓
projection et PRs selon policy
  ↓
ChangeSet backend pour le multi-repo
```

`session start`, `review plan`, `changeset create`, `review project` et
`review publish` restent des commandes de secours, d’inspection ou de
correction reviewer. Elles ne font pas partie de l’E2E Codex nominal.

### 0.2 Autorité des données

| Donnée | Autorité | Reconstructible depuis |
|---|---|---|
| Fichiers, trees, commits, branches, remotes | Git | objets Git et refs Git |
| Sessions, prompts, outils, écritures, validations | ledger Git signé | `refs/orkia/*` |
| Atomes, plans, `StackPullRequest`, `Stack`, `Projection` | refs Git signées | clone neuf + ledger |
| `ChangeSet` multi-repo, dépendances et état d’intégration | backend | enveloppes CLI signées + refs de stacks |
| Utilisateurs, organisations, OAuth, permissions | PostgreSQL | sauvegarde backend |
| Observations GitHub, outbox, jobs, receipts | PostgreSQL | journal opérationnel |
| PR Shape | service premium | preuves et entrées versionnées |

Le backend ne recopie jamais les patches Git dans son propre modèle de contenu.
Il valide et coordonne les références signées produites par les repositories.

### 0.3 Frontière Open-Core

Le core public contient le modèle, le ledger, Git, la capture, les agents,
l’analyse sémantique déterministe, les atomes, les stacks, le ChangeSet wire,
la projection et le CLI local. PR Shape est premium et ne doit pas être une
dépendance du chemin OSS.

Les fonctions cloud, multi-tenant, SSO/SCIM, analytics, flotte d’agents,
LLM/Visual Recap, broker de credentials et SLA sont séparées par API.

### 0.4 Règle de qualité

Chaque phase doit ajouter ses fixtures et ses tests avant d’exposer une nouvelle
commande. Une automatisation insuffisamment prouvée doit s’arrêter avec un
diagnostic explicite, jamais produire une stack ou une PR fragile.

## 1. Vue d’ensemble des phases

| Phase | Objectif de sortie | Dépend de |
|---|---|---|
| 0 | Trois dépôts GitHub, vertical slice réel, E2E Codex sans session manuelle | accès GitHub et environnement local |
| 1 | Frontières de crates et architecture vérifiable | Phase 0 démarrable |
| 2 | Contrat `orkia-wire` et ChangeSet backend | modèle et ports stabilisés |
| 3 | Chaîne automatique session → intention → atomes → stacks | hooks et wire |
| 4 | Projection exacte intra-fichier | atomes fermés et ports de patch |
| 5 | DAG mono-repo, restack et récupération | projection exacte |
| 6 | Forge, PRs automatiques et review protégée | stacks stables |
| 7 | ChangeSets multi-repo backend et wedge d’authentification | forge et wire |
| 8 | Benchmark, plateformes, licences et publication v0.1 | toutes les sorties précédentes |

Les phases 0 à 7 doivent être exécutées dans cet ordre. Les travaux premium
peuvent commencer en parallèle uniquement s’ils consomment un contrat déjà
versionné et ne modifient pas le core public.

## 2. Phase 0 — Bootstrap des dépôts et vertical slice réel

### 2.1 But

Obtenir rapidement un système exécutable sur trois dépôts GitHub réels, avec une
session Codex réelle, un backend local démarrable, des PRs de test et un premier
ChangeSet multi-repo. Ce n’est pas un POC jetable : chaque brique créée ici doit
être conservée, durcie ou remplacée dans le même changement.

### 2.2 Dépôts GitHub à créer

Sous l’organisation `orkiaHQ`, créer :

- `boop-orkia-backend` ;
- `boop-orkia-frontend` ;
- `boop-orkia-cli`.

Le préfixe `boop-` est temporaire. Il ne doit apparaître ni dans les noms de
crates, ni dans les IDs Orkia, ni dans les contrats wire, ni dans l’UI.

Pour chaque dépôt :

1. créer le repository avec visibilité décidée avant création ;
2. pousser un commit de référence identifiable ;
3. choisir `main` comme branche par défaut ;
4. ajouter licence, `README`, `CONTRIBUTING`, `SECURITY` et notices ;
5. ajouter CI format, tests, lint et build ;
6. protéger `main` sans autoriser de fusion automatique par les E2E ;
7. créer un préfixe de branches de test `boop/e2e/` ;
8. enregistrer l’URL et le SHA initial dans la documentation de validation.

Le backend actuel n’étant pas un dépôt Git, son premier commit doit précéder
toute migration de fichiers ou renommage.

### 2.3 Commande `orkia init`

Ajouter une commande de bootstrap repository qui compose les fonctions déjà
présentes :

1. vérifier que le chemin est un repository Git existant, ou créer Git avec une
   option explicite `--create-git` ;
2. créer ou vérifier `.git/orkia` ;
3. créer l’ID stable du repository ;
4. créer ou charger l’identité locale sans écraser une identité existante ;
5. vérifier les refs `refs/orkia/*` et le ledger ;
6. créer la policy par défaut si elle est absente ;
7. installer les hooks demandés (`--agent codex`) via `orkia-agents` ;
8. afficher un résumé idempotent : repository, actor, refs, agent, hooks,
   backend remote et policy.

`orkia init` ne doit pas lancer une session et ne doit pas inventer une
intention. Il prépare seulement le repository et l’environnement de capture.
`orkia identity init` et `orkia agent install` restent disponibles comme
opérations spécialisées.

Tests requis : repository vierge, clone existant, relance idempotente, identité
existante, hooks d’un autre outil, absence de permission, ref corrompue et
clone sans remote.

### 2.4 Bootstrap Codex et hooks

Le bootstrap réel est :

```sh
orkia init --agent codex --name "Orkia E2E"
orkia agent status --agent codex
# lancer Codex normalement dans le repository
```

Le test doit vérifier :

- hook `SessionStart` présent ;
- hooks d’actions présents et reconnus ;
- fonctionnalité Codex activée ;
- confiance persistante des commandes Orkia ;
- entrée Orkia identifiable dans la configuration ;
- installation répétée sans doublon ;
- désinstallation qui ne supprime pas les hooks d’autres outils.

L’installation est généralement globale au provider. Elle doit être faite une
fois par environnement et vérifiée pour chaque repository, pas réécrite de
manière destructive à chaque E2E.

### 2.5 Vertical slice backend

Le backend doit démarrer localement avec :

1. PostgreSQL et NATS via Docker Compose ;
2. migrations appliquées ;
3. serveur HTTP/GraphQL disponible ;
4. worker démarré ;
5. healthcheck et version wire publiés ;
6. authentification locale ou GitHub OAuth de test ;
7. registre de repositories contenant les trois `boop-*` ;
8. endpoint d’ingestion d’une enveloppe CLI signée ;
9. endpoint de statut d’un ChangeSet ;
10. événements SSE ou équivalent pour le frontend.

Le premier chemin peut utiliser un adaptateur de stockage minimal, mais il doit
respecter dès le départ les signatures, l’idempotence, l’autorisation et les
révisions append-only. Aucune table provisoire ne doit devenir une seconde
définition du modèle `ChangeSet`.

### 2.6 Vertical slice frontend

Le frontend doit :

1. être renommé Orkia dans package, README, cache et titre ;
2. se connecter au serveur local ;
3. afficher le repository courant et l’état de connexion ;
4. afficher la session et la preuve de capture ;
5. afficher les stacks et le ChangeSet multi-repo ;
6. afficher les révisions, dépendances et validations ;
7. expliquer les états inconnus, partiels ou bloqués ;
8. ne pas utiliser de données fictives dans le scénario E2E.

### 2.7 E2E Codex de référence

Le scénario de référence ne lance pas `orkia session start` :

1. cloner les trois dépôts `boop-*` dans trois répertoires isolés ;
2. exécuter `orkia init` dans chacun ;
3. lancer Codex normalement dans le backend ;
4. laisser Orkia créer la session, l’intention et les atomes ;
5. laisser Orkia créer le plan, les `StackPullRequest`, la `Stack` et le
   ChangeSet local ;
6. faire un changement réel dans le frontend ;
7. faire un changement réel dans le CLI ;
8. associer les trois stacks au ChangeSet multi-repo ;
9. déclencher l’authentification lors de la publication cross-repo ;
10. afficher le ChangeSet dans le frontend ;
11. créer les branches et PRs de test ;
12. vérifier les checks sans fusionner automatiquement `main` ;
13. reconstruire l’état depuis un clone neuf.

Chaque étape doit enregistrer session externe, session Orkia, actor, commits,
refs, stack revisions, projections, URLs de PR et validations.

### 2.8 Sortie de phase

La phase est bloquée si l’un de ces points échoue :

- dépôt non créé ou sans historique ;
- backend non démarrable depuis une commande documentée ;
- `orkia init` non idempotent ;
- session créée seulement par une commande manuelle ;
- atomes ou stacks nécessitant une déclaration de l’auteur ;
- ChangeSet multi-repo impossible après authentification ;
- PR réelle non reliée à sa projection ;
- état non reconstructible dans un clone neuf.

## 3. Phase 1 — Refactor de modularité et frontières de crates

### 3.1 But

Rendre les responsabilités testables indépendamment avant d’ajouter les
capacités multi-repo avancées.

### 3.2 Crates à stabiliser ou créer

Créer ou confirmer les frontières suivantes :

- `orkia-model` : types purs, IDs, événements, atomes, intentions, plans,
  stacks, projections, erreurs ; aucune I/O ;
- `orkia-codec` : canonicalisation JSON, versions de schéma, encodage et
  validation générique ;
- `orkia-ports` : ports fins pour ledger, refs, Git, patch, forge, wire,
  secrets, clock et validations ;
- `orkia-identity` : Ed25519, actor, signature et vérification ;
- `orkia-ledger` : hash chain, événements immuables et manifests ;
- `orkia-semantic` : hunks, symboles, Tree-sitter et dépendances ;
- `orkia-capture` : watchers, providers et événements ;
- `orkia-agents` : transcript, hooks, confiance et réconciliation ;
- `orkia-review` : graphe causal, confiance et corrections ;
- `orkia-changesets` : DAG, transitions, invariants et fermeture des preuves ;
- `orkia-projection` : patch exact, worktree, index temporaire et opérations ;
- `orkia-policy` : policy pure, seuils et validations ;
- `orkia-forge` : contrat forge neutre ;
- `orkia-github` : transport GitHub seulement ;
- `orkia-git` : implémentation libgit2 seulement ;
- `orkia-index-postgres` : projection reconstruisible ;
- `orkia-server` et `orkia-cli` : composition roots uniquement.

Créer aussi `orkia-access`, `orkia-vault` et `orkia-oci` si les capacités
correspondantes restent dans le périmètre, sans les laisser dans `orkia-git`.

### 3.3 Refactor d’`orkia-git`

Sortir de `orkia-git` :

1. la stratégie de merge métier ;
2. la décision de policy ;
3. la création de ChangeSets ;
4. les grants et équipes ;
5. le vault ;
6. l’orchestration des plans ;
7. les use cases de revue.

Conserver dans `orkia-git` : ouverture de repository, objets Git, refs,
worktrees, diff, index temporaire, commits et implémentation des ports.

### 3.4 Tests d’architecture

Ajouter un test CI qui échoue si un domaine importe :

- `git2` ;
- `std::fs` ou `std::process` ;
- HTTP/reqwest ;
- PostgreSQL/SQLx/SeaORM ;
- GitHub ;
- une crate d’implémentation.

Ajouter un graphe Cargo vérifié et un test contractuel par port contre :

1. une implémentation libgit2 ;
2. un double mémoire déterministe ;
3. un cas d’erreur et de retry.

### 3.5 Sortie de phase

La phase est terminée lorsque les cycles de dépendances sont impossibles,
`orkia-git` ne contient plus de décisions métier et les tests de phase 0
continuent de passer sans migration silencieuse.

## 4. Phase 2 — Contrat `orkia-wire` et ChangeSet backend

### 4.1 But

Permettre au CLI de produire des preuves et des stacks signées, puis au backend
de coordonner les `ChangeSet` multi-repo sans devenir un second Git.

### 4.2 Objets wire

Définir et versionner :

- `RepositoryDescriptor` ;
- `ActorDescriptor` ;
- `SessionEnvelope` ;
- `IntentEnvelope` ;
- `StackManifest` ;
- `ProjectionManifest` ;
- `ChangeSetSubmission` ;
- `ChangeSetRevision` ;
- `ValidationReceipt` ;
- `ForgeProjectionStatus` ;
- `CapabilitySet` ;
- erreurs et raisons de refus.

Chaque enveloppe doit contenir schema version, ID stable, revision, source
refs, digest des entrées, actor, signature, timestamps et liens de provenance.

### 4.3 `ChangeSetSubmission`

Le CLI envoie au backend :

1. identité du ChangeSet ;
2. repository IDs ;
3. Stack IDs et révisions exactes ;
4. dépendances directes et fermeture transitive ;
5. refs ou OIDs vérifiables ;
6. sessions et preuves nécessaires ;
7. policy digest ;
8. signature de l’actor ;
9. demande d’action : draft, publish, restack ou integrate.

Le payload ne contient pas de copie concurrente des fichiers ou patches.

### 4.4 Backend ChangeSet

Créer un modèle distinct de l’actuel détecteur Riftr :

- `change_set_revisions` ;
- `change_set_members` référencés par repository/stack/revision ;
- `change_set_dependencies` ;
- `change_set_validations` ;
- `change_set_projections` ;
- `change_set_events` ;
- `change_set_detections` pour les suggestions algorithmiques séparées.

Les révisions sont append-only. Une correction crée une nouvelle révision et
ne modifie jamais l’historique signé reçu du CLI.

### 4.5 Authentification wedge

Le CLI ne demande pas de login pour une stack mono-repo locale. Il demande une
authentification lorsqu’une opération nécessite :

- plusieurs repositories ;
- publication dans le control plane ;
- invitation d’une équipe ;
- coordination GitHub distante ;
- historique ou intégration centralisée.

Le message doit indiquer la raison, les repositories ciblés et les données
transmises. Un export local doit rester possible.

### 4.6 Tests et sortie

Tester signature valide/invalide, replay, duplication, ordre des refs,
dépendance manquante, permission insuffisante, repository inconnu, revision
superseded, clone neuf et export/import. La sortie est un ChangeSet backend
reconstructible à partir des manifests signés.

## 5. Phase 3 — Chaîne automatique de capture et de sémantique

### 5.1 But

Supprimer la déclaration manuelle de session, intention, atomes, stacks et
ChangeSets dans le parcours agent.

### 5.2 Session automatique

À `SessionStart` :

1. sélectionner le repository depuis `cwd` fiable ;
2. lire l’identifiant externe du provider ;
3. rattacher une session existante si le couple provider/session est connu ;
4. sinon créer une SessionId Orkia ;
5. enregistrer base commit, actor, provider et model si disponibles ;
6. créer un événement `AgentSessionLinked` ;
7. ne pas demander d’objectif bloquant.

À `SessionEnd` : clôturer la phase du provider, conserver le snapshot final et
laisser la policy décider de la suite.

### 5.3 Intent automatique

Le premier prompt utilisateur et les prompts suivants doivent produire un
objet `Intent` versionné, avec :

- texte original et digest ;
- session et actor ;
- timestamp ;
- objectif normalisé sans hallucination ;
- liens vers les événements sources ;
- statut `observed`, `partial` ou `unknown` ;
- révisions si l’intention évolue.

Une intention ne doit jamais être déduite uniquement du nom de l’agent ou de
l’ID externe. Ces valeurs restent des métadonnées de session.

### 5.4 Actions et transcripts

Conserver le document source exact, puis normaliser :

- prompts ;
- tours ;
- appels et résultats d’outils ;
- fichiers lus ;
- fichiers écrits ;
- commandes et tests ;
- erreurs et validations ;
- coûts/tokens lorsque le provider les expose.

Réconcilier les transcripts croissants par suffixe vérifiable et préfixe
d’actions. Une réécriture ambiguë est conservée comme révision brute et bloque
la confiance, sans dupliquer les actions.

### 5.5 Atomes automatiques

À chaque checkpoint automatique :

1. calculer le diff depuis la base session ;
2. fermer les événements sources par fichier et plage ;
3. extraire les hunks, blocs, symboles, imports, tests et configurations ;
4. calculer les dépendances dures et souples ;
5. créer des atomes avec contenu attendu, contexte, digest et preuves ;
6. marquer les écritures inconnues ;
7. calculer couverture et confiance ;
8. refuser la stack si les preuves sont insuffisantes.

### 5.6 Stacks et ChangeSet automatiques

Lorsque la policy passe :

1. partitionner les atomes en unités déterministes ;
2. fermer chaque unité sur ses dépendances ;
3. créer ou réviser les `StackPullRequest` ;
4. construire le `Stack` topologique du repository ;
5. créer le ChangeSet local de compatibilité ;
6. produire l’enveloppe backend si d’autres repositories sont liés ;
7. préparer les projections sans publier de contenu ambigu.

Une couverture insuffisante produit un diagnostic et conserve le Git normal.

### 5.7 Tests et sortie

Tester session start/stop, double hook, session provider réutilisée, prompt
réécrit, transcript croissant, écriture inconnue, action sans fichier, tests
échoués et policy sous seuil. L’E2E nominal ne contient aucun `session start`.

## 6. Phase 4 — Projection exacte intra-fichier

### 6.1 But

Projeter plusieurs ChangeSets dans un même fichier sans recopier les
modifications des autres unités et sans utiliser le chemin de fichier comme
frontière unique.

### 6.2 Patchs fermés

Chaque patch doit porter :

- path ;
- contenu ou digest avant ;
- contexte borné ;
- lignes attendues ;
- digest après ;
- IDs des atomes ;
- session et événements sources.

Refuser l’application si le contenu de base, le contexte ou le digest ne
correspond plus.

### 6.3 Matérialisation

Pour chaque unité :

1. créer un worktree isolé ;
2. créer un index temporaire ;
3. appliquer seulement les patchs fermés ;
4. vérifier le diff exact contre la fermeture des atomes ;
5. créer le commit projeté ;
6. créer la branche `orkia/cs/<ChangeSetId>` ou le nom policy ;
7. publier une projection signée ;
8. supprimer le worktree temporaire après succès ou conserver le diagnostic.

### 6.4 Tests et sortie

Tests requis : deux unités dans le même fichier, hunk voisin, hunk déplacé,
base modifiée, conflit, retry, interruption, worktrees concurrents, amend et
clone neuf. La sortie est un commit dont le diff est exactement celui des
atomes sélectionnés.

## 7. Phase 5 — DAG mono-repo et restack

### 7.1 But

Créer, modifier et reconstruire une stack ordonnée sans déclaration manuelle de
l’auteur.

### 7.2 Graphe

Construire la fermeture complète des dépendances, y compris plusieurs parents.
Ne jamais choisir un parent par tri lexical ou par numéro de PR. Les cycles
sont refusés avec un diagnostic signé.

### 7.3 Restack

Lorsqu’un parent change :

1. créer une nouvelle revision du `StackPullRequest` parent ;
2. reprojeter son commit ;
3. recalculer la base de chaque descendant ;
4. reprojeter les descendants dans l’ordre topologique ;
5. enregistrer les projections superseded ;
6. arrêter uniquement la branche aval concernée par un conflit ;
7. conserver les autres unités valides.

### 7.4 Corrections reviewer

Merge, split, reorder, squash, abandon et reprise créent une revision signée.
Le ledger causal ne change jamais. Une correction ne peut pas réutiliser
silencieusement une revision d’atome ancienne.

### 7.5 Tests et sortie

Fixtures obligatoires : stack de trois niveaux, amend parent, multi-parent,
split, squash, reorder, abandon, reprise, conflit aval, perte de cache et
réception des refs dans un ordre différent.

## 8. Phase 6 — Forge, PRs et intégration protégée

### 8.1 But

Publier automatiquement une PR par projection, sans que l’auteur déclare les
PRs empilées.

### 8.2 Publication selon policy

La policy détermine :

- publication automatique ou draft ;
- branche de base ;
- préfixe des branches ;
- checks requis ;
- approbations requises ;
- protection de `main` ;
- comportement en cas de conflit.

L’automatisation peut créer/mettre à jour les branches et PRs, mais ne fusionne
jamais `main` sans la décision d’intégration autorisée.

### 8.3 Forge neutre et GitHub

Le port forge doit couvrir création, mise à jour, base, labels, body, checks,
reviewers, webhook et fermeture. `orkia-github` ne contient aucune logique de
partition ou de causalité.

Tester d’abord avec un double forge, puis avec une GitHub App de test. Le test
réel doit vérifier JWT, installation token, check-run, PRs dépendantes,
webhook, retry et branche protégée.

### 8.4 Sortie

Une stack est visible comme PRs ordonnées, chaque PR est reliée à son
`StackPullRequest` et sa projection, et l’intégration topologique est bloquée
si un check ou une approbation manque.

## 9. Phase 7 — ChangeSet multi-repo et plateforme

### 9.1 But

Faire du ChangeSet multi-repo le wedge de connexion à la plateforme, sans
verrouiller le travail local.

### 9.2 Détection automatique multi-repo

Le backend doit pouvoir recevoir plusieurs soumissions provenant de sessions
ou repositories différents et :

1. vérifier les signatures ;
2. vérifier l’existence des repository IDs ;
3. vérifier chaque Stack revision ;
4. fermer les dépendances cross-repo ;
5. créer ou réviser le ChangeSet ;
6. détecter les collisions sans les confondre avec la causalité ;
7. calculer l’ordre d’intégration ;
8. publier les statuts et événements ;
9. demander une authentification seulement au premier besoin de plateforme.

### 9.3 Authentification et permissions

Les permissions doivent couvrir organisation, projet, repository, stack et
action. Les erreurs doivent distinguer repository absent, accès refusé,
signature invalide, revision obsolète et policy bloquante.

Le CLI doit offrir login, logout, status, changement d’organisation et export
local. Le token ne doit jamais être écrit dans un argument Git ou un log.

### 9.4 Intégration multi-repo

Le backend ordonne les stacks, mais chaque contenu reste projeté dans son
repository. L’intégration :

1. vérifie tous les ChangeSets dépendants ;
2. vérifie les projections forge ;
3. vérifie checks et approbations ;
4. exécute dans l’ordre topologique ;
5. enregistre chaque étape et chaque conflit ;
6. peut reprendre après interruption ;
7. ne mélange jamais les patches de deux repositories.

### 9.5 PR Shape premium

PR Shape ne doit pas être requis pour créer ou intégrer un ChangeSet. Le
service premium peut recevoir les preuves versionnées, calculer PR Shape,
retourner un résultat explicable et l’associer à la PR. Le CLI OSS doit afficher
`capability unavailable` ou `not requested`, jamais simuler le résultat.

### 9.6 Sortie

Un utilisateur peut travailler localement sans compte, puis se connecter au
moment de publier un ChangeSet multi-repo. Le ChangeSet est visible par les
collaborateurs autorisés, exportable, vérifiable et récupérable.

## 10. Phase 8 — Qualité, benchmark, licences et publication

### 10.1 Benchmark Ghost PR

Collecter des sessions Orkia causales réelles et mesurer sans annotation
humaine :

- uplift d’au moins 20 % sur le meilleur baseline ;
- moins de 10 % de paires corrigées séparées ;
- ARI au moins égal à 0,8 après split/squash.

Le cache historique commit-only ne peut pas servir de preuve de causalité.
Un gate fail-closed doit refuser la publication lorsque les données requises
manquent.

### 10.2 Matrice E2E

Sur macOS et Linux :

- Codex réel dans les trois dépôts `boop-*` ;
- capture humaine sans commande de session ;
- session incomplète ;
- transcript croissant et réécrit ;
- worktrees concurrents ;
- split, squash, amend et restack ;
- conflit et reprise ;
- clone neuf ;
- backend redémarré ;
- webhook en retry ;
- GitHub App réelle ;
- branche protégée ;
- PR Shape absent et premium disponible.

### 10.3 Build et supply chain

Uniformiser Rust et le lockfile. Exécuter sur chaque dépôt :

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --locked
git diff --check
```

Ajouter SBOM, provenance des crates MIT, scan des secrets, dépendances
reproductibles et artefacts signés. Exécuter Sigstore dans CI avant d’annoncer
une release.

### 10.4 Licence et packaging

Avant publication :

1. ajouter LICENSE/NOTICE aux dépôts ;
2. documenter Apache-2.0 CLI, AGPL backend et MIT imports ;
3. choisir la licence frontend ;
4. séparer clairement code premium et OSS ;
5. publier la matrice de capabilities ;
6. ajouter procédure de reconstruction depuis refs et sauvegardes backend.

### 10.5 Critère final v0.1

La v0.1 est publiable seulement si :

- les trois dépôts sont reproductibles ;
- `orkia init` est idempotent ;
- un agent démarre sans `session start` ;
- session, intention, atomes, stacks et ChangeSet sont automatiques ;
- le multi-repo se connecte au backend ;
- les PRs réelles sont créées et protégées ;
- les scénarios E2E réels et factices passent ;
- les seuils Ghost PR passent ;
- les limites premium et licences sont documentées ;
- les artefacts de release sont signés.

## 11. Garde-fous et décisions qui restent ouvertes

Les points suivants doivent rester visibles jusqu’à résolution :

- benchmark causal non validé ;
- test GitHub App réel des check-runs incomplet ;
- Claude en succès réel non validé ;
- Sigstore non exécuté ;
- backend actuellement non versionné dans Git ;
- lots backend Inbox et sessions agents non terminés ;
- frontend encore partiellement placeholder ;
- conflit documentaire à résoudre dans `changeset-stack-execution-plan.md`
  concernant l’autorité du ChangeSet ;
- PR Shape premium à maintenir hors du chemin OSS.

Une phase ne peut être marquée `complete` que lorsque ses sorties sont
observées dans les dépôts, les tests et les E2E. Une phase bloquée doit
documenter la cause externe précise et le scénario de reprise.
