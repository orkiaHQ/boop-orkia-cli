# Feuille de route — Uniformisation Orkia et frontière Open-Core

> Statut : feuille de route de convergence.
>
> Cette feuille complète `changeset-stack-execution-plan.md`. Elle ne remplace
> pas ses critères de sortie : les seuils Ghost PR, la causalité capturée, la
> projection exacte, le restack et la récupération restent obligatoires.

## 1. Décision directrice

Orkia doit être un moteur sémantique Git-native :

- Git reste l’autorité du contenu, des commits, des arbres, des branches et du
  transport ;
- les preuves, atomes, sessions, plans, stacks et projections locales sont
  canoniques, versionnés, signés et publiés dans `refs/orkia/*` ;
- le backend possède la coordination durable des `ChangeSet` et de leur état
  multi-repository ; chaque révision référence des stacks signées et peut être
  exportée sous forme de manifeste signé dans `refs/orkia/changesets/*` ;
- PostgreSQL sert les comptes, permissions, observations GitHub, jobs, outbox et
  index reconstruisibles ;
- le CLI local doit rester fonctionnel sans service hébergé ;
- le premium ajoute des services d’organisation et d’hébergement sans cacher
  le moteur causal ou le moteur de stacks.

Cette décision doit être confirmée par un ADR avant toute migration backend.

## 1.1 Positionnement produit : le ChangeSet multi-repo comme wedge

Le `ChangeSet` multi-repo doit être le point d’entrée vers la plateforme Orkia.
Le CLI reste utile et complet sans compte pour capturer une session, produire
des stacks et projeter du Git localement. La connexion devient nécessaire au
moment où l’utilisateur veut coordonner plusieurs dépôts, inviter une équipe,
publier des PRs dépendantes ou suivre l’intégration depuis un control plane.

Parcours cible :

```text
travail local
    ↓
capture causale + Stack mono-repo (OSS, offline)
    ↓
orkia changeset publish / integrate --multi-repo
    ↓
orkia auth login  ── organisation · projet · permissions
    ↓
ChangeSet backend : dépendances · validations · PRs · état d'intégration
    ↓
collaboration, historique et fonctions premium
```

Ce wedge doit respecter quatre règles :

1. La connexion n’est pas requise pour le travail Git local.
2. Le CLI explique précisément pourquoi une connexion est demandée et quelles
   données seront publiées.
3. Le backend reçoit des enveloppes signées et des références de stacks, jamais
   un second contenu Git concurrent.
4. Un `ChangeSet` peut être exporté, vérifié et récupéré sans dépendre d’une
   fonctionnalité premium propriétaire.

La conversion produit vient donc de la coordination multi-repo et de la valeur
collaborative, pas d’un verrouillage artificiel du moteur de causalité.

## 2. État actuel vérifié

### 2.1 Deux autorités concurrentes

Le CLI et le backend peuvent coexister si leur périmètre d’autorité est
explicite. Le CLI produit les preuves et les stacks signées ; le backend
coordonne les `ChangeSet` cross-repository, leur état, les dépendances et la
publication forge.

Décision cible :

| Donnée | Autorité |
|---|---|
| Fichiers, trees, commits, branches, remotes | Git |
| Ledger causal, atomes, plans, stacks, projections | Refs Git signées `refs/orkia/*` |
| Coordination `ChangeSet`, dépendances cross-repo, état et intégration | Backend PostgreSQL, alimenté par des enveloppes CLI signées |
| Utilisateurs, OAuth, permissions, organisations | PostgreSQL |
| Observations et mutations GitHub brutes | PostgreSQL, append-only |
| Jobs, outbox, receipts, état de worker | PostgreSQL/NATS |
| Index de recherche et projections UI | PostgreSQL, reconstruisible |

### 2.2 Collision de vocabulaire

Le `ChangeSet` du CLI est une unité durable de livraison composée de stacks
dépendantes, potentiellement multi-repo, sans contenu Git propre.

Le `change-set-engine` backend détecte actuellement des groupes de PR/issues,
des collisions et des signaux de coordination GitHub. Ce résultat est une
projection ou une suggestion, pas le `ChangeSet` canonique.

Le concept backend actuel doit être renommé en `ChangeSetDetection`,
`CoordinationCluster` ou `CoordinationSuggestion`. Il restera un signal
d’entrée. Le futur service backend `ChangeSet` sera un objet distinct, créé à
partir de stacks et de preuves signées reçues du CLI.

### 2.3 Absence de contrat CLI ↔ backend

Le backend ne possède pas encore de contrat pour les refs signées, les
`StackPullRequest`, `Stack`, `Projection`, `ReviewPlan` ou la capture causale.
Les méthodes actuelles exposent surtout des projections de repository, de scan
et de change-set détecté.

Il faut introduire un contrat public versionné `orkia-wire`, indépendant de
PostgreSQL, de GraphQL et du CLI.

### 2.4 Modularité incomplète dans le CLI

Le workspace CLI possède déjà les crates de modèle, ledger, Git, capture,
agents, revue, projection, ChangeSets, forge et GitHub. La phase 1 du plan
reste toutefois incomplète : les frontières `orkia-codec`, `orkia-access`,
`orkia-vault` et `orkia-oci` ne sont pas encore matérialisées.

`orkia-git` contient encore des responsabilités métier qui doivent sortir de
l’adaptateur Git : policy, merge métier, vault, grants et orchestration de
plans.

Les fixtures prévues `tests/atomic-parity/` et `tests/changeset-stacks/` restent
à créer.

### 2.5 Hygiène de dépôt et de build

- Le backend n’est pas actuellement un dépôt Git.
- Le frontend est un dépôt Git mais possède des modifications non commitées et
  conserve plusieurs noms Riftr (`riftr-linear-github`, `riftr-cache`).
- Le README backend et `scripts/dev-stack.zsh` pointent vers un ancien chemin
  frontend.
- Le CLI déclare Rust 1.93 ; le backend déclare Rust 1.85/stable alors que ses
  dépendances actuelles nécessitent Rust 1.94. Les tests backend passent avec
  Rust 1.94, pas avec la version déclarée.

## 3. Frontière Open-Core

### 3.1 À maintenir public

Ces composants doivent rester open source et auditables :

- `orkia-model`, `orkia-wire`, ports et formats canoniques ;
- identités Ed25519, signatures et vérification ;
- ledger causal et stockage Git des refs ;
- adaptateur Git/libgit2 ;
- analyse sémantique déterministe ;
- capture humaine et support Codex/agents ;
- revue, partition, confiance, politiques et corrections reviewer ;
- `StackPullRequest`, `Stack` et `Projection` ;
- schéma public `ChangeSet`, enveloppes signées et validation de dépendances ;
- projection exacte, restack, récupération et worktrees ;
- contrat forge et adaptateur GitHub de base ;
- CLI offline/local-first ;
- serveur self-hosted minimal si distribué, avec une licence explicite ;
- moteurs déterministes de scan repris de Riftr, avec notices MIT.

Le moteur causal et le moteur de stacks ne doivent pas devenir des fonctions
premium : ce sont les garanties vérifiables du CLI.

### 3.2 À réserver au premium

Le code privé doit être additif et communiquer avec le core par `orkia-wire` :

- control plane hébergé multi-tenant ;
- SSO, SCIM, RBAC avancé et politiques organisationnelles ;
- rétention cloud, index hébergé et recherche transverse ;
- gestion de flotte d’agents, orchestration distante et analytics de sessions ;
- PR Shape, dashboards CTO, analytics organisationnels et runtime/ClickHouse ;
- LLM hébergés, Visual Recap et fonctions sémantiques coûteuses ;
- broker de credentials GitHub/LLM, rotation managée, SLA et support ;
- provisionnement de clones et infrastructure cloud opérée.

Le moteur de stacks et le contrat `ChangeSet` restent publics. PR Shape est une
capacité premium : le CLI ne doit ni l’exécuter localement ni en dépendre pour
créer une stack. Le service premium peut l’exécuter, conserver ses preuves et
publier un résultat explicable via `orkia-wire`.

### 3.3 Règles de licence

- Le CLI et les contrats partagés doivent conserver une licence permissive
  explicite, actuellement Apache-2.0.
- Les composants backend actuels déclarent AGPL-3.0-only ; ils doivent recevoir
  un fichier `LICENSE` et des notices à la racine.
- Les crates dérivées de Riftr sous MIT doivent conserver leurs copyrights,
  `SOURCE.md` et notices MIT.
- Le frontend doit recevoir une licence explicite avant d’être présenté comme
  open source.
- Le premium ne doit pas être une modification privée distribuée d’un binaire
  AGPL sans analyse juridique. Il doit rester un service ou un dépôt séparé.

## 4. Architecture cible

```text
orkia-model / orkia-wire
        ↓
CLI local ── capture · ledger · semantic · review · stacks · projection
        ↓ refs/orkia/* + enveloppes signées
Backend self-hosted ── ChangeSets · auth · observations · index · jobs · GitHub
        ↓
Frontend Orkia

Services premium séparés ── cloud · fleet · analytics · LLM · credentials
```

Le backend ne doit pas importer le serveur du CLI ni l’inverse. Les deux
assemblent les mêmes contrats publics et possèdent leurs propres adaptateurs.

## 5. Plan de convergence

### Phase 0 — Figer les décisions

1. Écrire l’ADR Git/PostgreSQL.
2. Figer le vocabulaire `Stack`, `StackPullRequest`, `Projection` et
   `ChangeSet`.
3. Renommer le détecteur backend pour supprimer l’ambiguïté.
4. Choisir la licence du backend et du frontend.
5. Initialiser le dépôt Git backend et committer un état de référence.
6. Acter le `ChangeSet` multi-repo comme wedge d’authentification et d’adoption
   de la plateforme : le travail mono-repo reste offline, tandis que la
   publication et l’intégration cross-repo déclenchent `orkia auth login`.
7. Définir l’enveloppe signée `StackManifest`/`ChangeSetSubmission` que le CLI
   envoie au backend. Elle contient les identités, révisions, dépendances et
   preuves nécessaires, mais aucun contenu Git concurrent.
8. Acter que le backend possède l’état canonique des `ChangeSet` cross-repo,
   avec révisions append-only et export signé récupérable.
9. Acter PR Shape comme capability premium : elle est absente du chemin OSS de
   création, projection et intégration d’un ChangeSet.
10. Mettre à jour `changeset-stack-execution-plan.md` pour remplacer toute
    règle affirmant que le `ChangeSet` multi-repo est exclusivement canonique
    dans les refs Git ; les refs restent canoniques pour les preuves, stacks et
    projections.
11. Créer sous l’organisation GitHub `orkiaHQ` les dépôts temporaires suivants :
    - `boop-orkia-backend` ;
    - `boop-orkia-frontend` ;
    - `boop-orkia-cli`.
    Le préfixe `boop-` est provisoire et ne doit pas contaminer les noms de
    crates, les contrats wire ou les identifiants produit.
12. Définir pour chacun une branche par défaut, une licence, les protections de
    branche et les workflows CI minimaux. Le backend doit recevoir un premier
    commit source avant toute migration ; le frontend doit recevoir son état
    actuel nettoyé ; le CLI doit recevoir le workspace déjà testé.
13. Prioriser un vertical slice exécutable sur ces trois dépôts avant les
    extractions de modularité non bloquantes : démarrage backend, connexion du
    CLI, publication d’une stack, soumission d’un `ChangeSet` multi-repo,
    affichage frontend et intégration contrôlée.
14. Préparer les fixtures et scripts E2E utilisant de vraies sessions Codex,
    un dépôt Git local par repository, des refs signées, un backend réel et le
    frontend connecté. Chaque étape doit conserver les identifiants de session,
    commits, projections, PRs et validations.
15. Ajouter la commande de bootstrap repository `orkia init` comme composition
    idempotente des mécanismes existants : `orkia identity init`, création des
    métadonnées `.git/orkia` et, sur demande, `orkia agent install --agent
    codex`. Elle doit exposer un statut clair sans réimplémenter la logique de
    hooks déjà portée par `orkia-agents`.
16. Acter que le parcours agent nominal ne contient ni `session start`, ni
    déclaration manuelle d’intention, de stack ou de ChangeSet : `SessionStart`
    crée ou rattache automatiquement la session, le premier prompt produit
    l’intention, les changements capturés sont transformés en atomes,
    `Stop`/`SessionEnd` déclenche le checkpoint et le planner, puis les
    `StackPullRequest`, le `Stack` et le `ChangeSet` sont créés automatiquement.
    Une politique autorisée déclenche ensuite la projection, la publication des
    PRs et la soumission backend.

**Sortie :** décisions opposables, trois dépôts GitHub `boop-*` créés, un
vertical slice local démarrable et un premier scénario E2E Codex reproductible.

### Priorité opérationnelle de la Phase 0

L’ordre d’exécution est volontairement orienté usage réel :

1. créer et initialiser les trois dépôts GitHub ;
2. exécuter `orkia init` dans chaque clone local des trois dépôts ; vérifier
   ensuite `orkia agent status --agent codex` et installer les hooks uniquement
   si le statut l’indique ;
3. faire démarrer le backend avec sa base et ses migrations ;
4. connecter le CLI au backend via le contrat minimal ;
5. exécuter une session Codex réelle et publier ses stacks ;
6. créer un `ChangeSet` multi-repo après authentification ;
7. afficher son état dans le frontend ;
8. exécuter l’intégration et vérifier les protections ;
9. seulement ensuite poursuivre les refactors qui ne sont pas nécessaires à ce
   vertical slice.

Les refactors de modularité restent obligatoires, mais aucune abstraction ne
doit retarder la capacité à produire et observer ces cas d’usage E2E.

### Bootstrap agent déjà disponible

Le CLI possède déjà les primitives nécessaires :

```sh
orkia identity init --name "Orkia E2E"
orkia agent install --agent codex
orkia agent status --agent codex
# Lancer Codex normalement dans le repository initialisé
```

`agent install` est idempotent, fusionne les hooks d’autres outils et persiste
la confiance Codex. `agent hook` est le point d’entrée natif fail-safe invoqué
par le fournisseur. `orkia init` doit donc devenir une façade de bootstrap
repository, pas une deuxième implémentation des hooks.

`orkia session start` reste disponible uniquement pour le mode humain surveillé,
les tests ciblés, la reprise après panne ou un override explicite. Il ne doit
pas apparaître dans le scénario E2E Codex nominal.

### Automatisation déjà présente et écarts à fermer

Le code actuel possède déjà une partie du parcours automatique :

- `SessionStart` crée une session Orkia et lie l’identifiant externe du
  fournisseur ;
- les prompts et actions capturés alimentent automatiquement les atomes
  sémantiques, avec leurs événements de preuve ;
- `Stop`/`SessionEnd` capture un snapshot, calcule la couverture causale et
  déclenche le plan de revue lorsque la policy est satisfaite ;
- la persistance du plan crée déjà automatiquement les `StackPullRequest`, le
  `Stack` et un `ChangeSet` local signé.

Les écarts ne doivent pas être masqués : l’objectif généré est encore une
chaîne générique basée sur l’agent et l’identifiant externe, l’intention n’est
pas encore un objet déduit du premier prompt, la publication forge n’est pas
encore automatique, et le ChangeSet multi-repo backend n’est pas encore soumis
automatiquement.

Les commandes `review plan`, `changeset create`, `review project` et `review
publish` restent des surfaces d’inspection, de reprise ou de correction
reviewer. Elles ne doivent pas être nécessaires au parcours agent nominal.

### Scénarios E2E réels prioritaires

Les scénarios de référence utilisent les dépôts GitHub `boop-*`, les clones
locaux correspondants et de vraies sessions Codex :

1. initialisation des trois dépôts par `orkia init` ;
2. lancement normal de Codex, sans `session start` ni objectif déclaré à Orkia ;
3. détection automatique de session, intention, actions, atomes, checkpoint,
   plan, stacks et ChangeSet ;
4. changement réel dans le backend, frontend et CLI, avec PRs produites par la
   politique de publication ;
5. soumission automatique d’un `ChangeSet` multi-repo après authentification ;
6. revue, amend amont, reprojection et restack des PRs aval ;
7. intégration topologique avec checks et branche protégée ;
8. récupération depuis un clone neuf et reconstruction de l’index.

Ces tests doivent produire de vraies branches et PRs, mais ne doivent jamais
fusionner automatiquement dans `main` sans validation explicite. Les branches
de test restent préfixées et les dépôts `boop-*` constituent la zone isolée de
validation produit.

### Scénarios complémentaires sur dépôts factices

Des dépôts Git éphémères restent nécessaires pour les cas destructifs ou
hautement répétitifs : conflits intra-fichier, commits mal ordonnés, perte de
cache, transcripts réécrits, retries, pannes réseau, worktrees concurrents,
signatures invalides et tests de migration. Ils complètent les E2E réels, mais
ne les remplacent pas.

### Phase 1 — Refactor de modularité du CLI

1. Créer `orkia-codec` pour canonicalisation et versions de schéma.
2. Extraire `orkia-access`, `orkia-vault` et `orkia-oci`.
3. Réduire `orkia-git` à libgit2 et aux ports Git.
4. Réduire `orkia-ports` à des interfaces orientées capacité.
5. Ajouter le test d’architecture des dépendances.
6. Ajouter les tests contractuels Git réel + double mémoire.

**Sortie :** aucun domaine ne dépend du filesystem, de Git, HTTP ou PostgreSQL.

### Phase 2 — Contrats partagés

1. Créer `orkia-wire` à partir des types stables du modèle.
2. Versionner JSON, IDs, signatures, capacités et erreurs.
3. Définir les endpoints de refs, index, sessions, plans, stacks et ChangeSets.
4. Définir l’enveloppe signée par laquelle le CLI alimente un `ChangeSet` backend
   sans transmettre de contenu Git concurrent.
5. Ajouter la négociation de capacités pour distinguer OSS et premium.

**Sortie :** le backend peut indexer le CLI sans importer sa logique métier.

### Phase 3 — Pont backend et refs signées

1. Ajouter fetch/push des refs `refs/orkia/*` pour les preuves, stacks et
   projections.
2. Persister le `ChangeSet` de coordination dans PostgreSQL : identité stable,
   révisions append-only, dépendances, validations, état et liens vers les
   stacks signées. Aucun patch ou fichier n’est recopié.
3. Reconstruire l’index depuis un clone neuf.
4. Ajouter les tables de sessions agents et l’attribution durable.
5. Exposer les objets canoniques dans GraphQL/HTTP.

**Sortie :** une stack créée par le CLI alimente un `ChangeSet` backend sans
perdre sa signature, sa causalité ni ses révisions ; un clone peut récupérer les
stacks et le backend peut reconstruire la coordination.

### Phase 4 — Frontend uniforme

1. Renommer le package et les caches Riftr en Orkia.
2. Corriger les chemins de développement et la documentation.
3. Ajouter les vues réelles Stack, ChangeSet, Projection, session et review.
4. Afficher les preuves, révisions et limites de confiance.

**Sortie :** aucune vue métier ne repose sur des données fictives ou un second
vocabulaire.

### Phase 5 — Produit premium séparé

1. Créer le dépôt/service privé premium.
2. Utiliser exclusivement `orkia-wire` pour les fonctions privées.
3. Ajouter PR Shape comme capability premium explicite, avec refus propre si le
   service n’est pas disponible.
4. Ajouter `orkia capabilities` et des messages d’erreur explicites dans le
   CLI.
5. Ajouter CI de compatibilité wire entre versions OSS et premium.

**Sortie :** le CLI public reste complet localement ; le premium ajoute de la
valeur sans rendre le core opaque.

## 6. Garde-fous du plan ChangeSets

Cette uniformisation ne doit pas masquer les sorties encore manquantes :

- benchmark Ghost PR : seuils de +20 %, moins de 10 % de paires séparées et
  ARI ≥ 0,8 non atteints ;
- sessions causales exportées en volume suffisant pour le benchmark ;
- fixtures atomic-parity et changeset-stacks absentes ;
- synchronisation distante des refs non livrée ;
- test GitHub App réel des check-runs non réalisé ;
- validation Claude réussie non obtenue ;
- publication Sigstore non exécutée ;
- lots backend Inbox, sessions agents, LLM et Runtime encore ouverts.

La version 0.1 ne doit être annoncée qu’après validation de ces critères dans
`changeset-stack-execution-plan.md`.

## 7. Prochaine action autorisée

La prochaine implémentation doit être la Phase 0 puis la Phase 1 : dépôt
backend, ADR d’autorité, licences, nomenclature et refactor de modularité du
CLI. Aucune nouvelle fonctionnalité premium ne doit être développée avant que
ces frontières soient testées et documentées.
