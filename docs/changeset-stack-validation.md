# État de validation — ChangeSets et stacks

Ce document sépare les preuves produites par le code et les exécutions locales
des conditions de sortie qui demandent des données de production. Il ne doit
pas être interprété comme une déclaration de release tant que chaque ligne de
la seconde section n’est pas couverte.

## Preuves exécutables dans ce worktree

- `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo fmt --check` et `git diff --check` couvrent les crates et
  les contrats locaux.
- `scripts/e2e_codex_local.sh` crée un dépôt Git vierge, exécute une vraie
  session Codex, vérifie le ledger signé, projette une StackPullRequest et
  reconstruit le plan depuis un clone miroir ne contenant pas le cache de
  worktree. Il exerce aussi un passage d’intégration réussi sur une branche
  non protégée, dont la décision est signée dans le ledger. Le même test crée un ChangeSet puis vérifie que
  `integrate --changeset` refuse une projection locale qui n’est pas encore
  publiée par la forge.
- `orkia-projection` vérifie avec un dépôt libgit2 réel qu’un amendement de
  StackPullRequest amont reprojette son descendant sans réécrire la branche de
  base.
- `orkia-semantic` filtre les bornes Tree-sitter par les hunks concrets du diff
  : un symbole inchangé entre deux hunks ne devient pas une unité fantôme. Les
  corrections de plan et les projections récupèrent également la session
  source signée du plan, jamais une session plus récente.
- Les fichiers de configuration et de migration produisent explicitement des
  atomes `Configuration` ou `Migration`; les blocs de code sans frontière de
  symbole restent des atomes `Block`.
- `orkia-git` couvre la persistance signée et la reconstruction de Stacks et
  ChangeSets multi-dépôts via `refs/orkia`.
- Le statut serveur d’un ChangeSet résout les révisions exactes des Stacks,
  StackPullRequests et projections. Une projection issue d’un restack plus
  récent ne peut donc pas rendre artificiellement prête une stack historique.
  Son contrôle parcourt aussi toute la fermeture des ChangeSets dépendants et
  bloque lorsqu’un groupe amont n’a pas ses projections forge-publiées.
- `orkia changeset status` exécute la même reconstruction en local. Il a été
  exécuté sur le dépôt Git réel de la preuve Codex et a correctement indiqué
  `ready_for_integration: false` pour une Stack projetée mais non publiée vers
  une forge, avec son `execution_order` topologique.
- Le test d’intégration CLI marque aussi une projection avec une URL de forge
  signée, exécute réellement `integrate --changeset` dans un dépôt temporaire
  et vérifie que le ChangeSet passe à une révision `Integrated`.
- `orkia-github` exerce un serveur HTTP local pour l’échange GitHub App JWT
  RS256, les checks, la mise à jour de PR et la protection de branche avec le
  quorum défini par la politique.
- Le serveur valide les webhooks GitHub, les déduplique contre le retry en
  mémoire et contre le ledger durable, puis persiste chaque livraison comme un
  événement `forge_webhook` signé avant de répondre. Avec plusieurs dépôts
  enregistrés, `x-orkia-repository` est obligatoire pour conserver la
  provenance.
- Le token de service (`ORKIA_SERVICE_TOKEN`) est vérifié avant les endpoints
  de statut et permet au control plane de reconstruire les refs sans usurper
  un grant de reviewer humain.
- `scripts/e2e_server_webhook.sh` exerce cette route avec un serveur compilé,
  un dépôt Git neuf, une signature HMAC réelle, un accusé `202`, un retry
  `204` et une vérification complète du ledger.
- `scripts/e2e_human_watcher.sh` démarre une session humaine réelle, attend le
  watcher persistant macOS/Linux, modifie un fichier hors de `orkia run`, puis
  vérifie l’événement `unknown_write` signé et le refus de génération de stack.
- `scripts/e2e_server_changeset_status.sh` reconstruit un ChangeSet depuis un
  dépôt Git neuf, lance le serveur avec un bearer de service et vérifie en HTTP
  le statut `200`, `ready_for_integration: false` et l’ordre topologique.
- `scripts/e2e_github_protected.sh` a créé le dépôt public réel
  `killix/orkia-protected-e2e-202608011844`, a poussé une branche avec
  libgit2 via HTTPS, puis a publié la PR
  `https://github.com/killix/orkia-protected-e2e-202608011844/pull/1` avec
  `review publish`. L’API GitHub confirme `orkia/integrate`, une approbation
  requise et `enforce_admins`; GitHub expose `mergeStateStatus: BLOCKED` sans
  check ni approbation.
- Les 17 crates déclarent la même version minimale Rust 1.93 ; `verify.yml` et
  `release.yml` utilisent ce toolchain, et le binaire CLI macOS arm64 a été
  recompilé après cette harmonisation.
- Les écritures ledger et objets signés sont idempotentes pour une répétition
  byte-identique et refusent toute tentative de réutiliser une révision avec
  un contenu différent.
- Un ChangeSet intégré reçoit une révision signée `Integrated`; les ChangeSets
  dépendants refusent désormais un groupe seulement publié mais non intégré.
- L’image serveur se construit localement avec `docker build -t
  orkia-server:changesets-local .`. Elle s’exécute sous l’utilisateur non-root
  `orkia` et son endpoint `GET /health` a répondu `status: ok` sur un port
  local. Le `.dockerignore` exclut explicitement les sorties de build et les
  données Git locales du contexte de l’image.
- Le binaire CLI macOS arm64 a été compilé avec `cargo +1.93.1 build --locked
  --release --target aarch64-apple-darwin -p orkia-cli` et son aide s’exécute
  correctement. La publication signée par Sigstore reste confiée au workflow
  GitHub Actions, qui n’a pas été déclenché localement.

## Conditions de sortie non encore prouvées

1. Le benchmark Ghost PR doit obtenir, avec des sessions causales Orkia
   capturées avant publication, un gain d’au moins 20 %, moins de 10 % de
   paires séparées et un ARI d’au moins 0,8. Le gate
   `scripts/verify_ghost_pr_thresholds.py` applique ces seuils sans
   assouplissement.
2. Le cache historique disponible est un benchmark rétrospectif de commits et
   de feedback GitHub. Il n’a ni transcript, ni appels d’outils, ni fichiers
   lus, ni identifiant de session. Il ne peut donc pas évaluer le modèle
   `causalité capturée ∩ dépendances sémantiques`. Son résultat Commit +
   Jaccard est volontairement refusé par le gate : +3,3 %, 42,2 % de paires
   séparées et ARI 0,441.
3. Le blocage GitHub externe est maintenant prouvé sur un dépôt public réel
   avec le chemin d’injection d’un token d’installation. L’échange RS256 avec
   une GitHub App réellement installée n’a pas été exécuté faute de credentials
   App dédiés; il reste couvert par le serveur HTTP contractuel. Les
   credentials de signature macOS/Linux restent nécessaires pour prouver la
   publication de binaires signés.
4. La validation d’édition réussie par Claude Code est reportée à la demande
   de l’utilisateur en raison du quota hebdomadaire du fournisseur. Le binaire
   local a néanmoins été invoqué par `session agent --provider claude` : son
   transcript HTTP 429 a été signé dans le ledger, ce qui prouve le
   comportement fail-closed et la conservation de l’échec, mais pas une
   édition Claude réussie.

La collecte requise pour le point 1 est donc : des sessions Orkia exportées
avec leurs refs signées, reliées à des PRs dont la première review humaine et
les corrections ultérieures sont disponibles. Sans cette liaison temporelle,
un score qui prétendrait mesurer la causalité serait une inférence postérieure
et non une validation.
