export PATH="$PWD/target/release:$PATH"
export AIRBAG_HOME=/tmp/shop-airbag
rm -rf /tmp/shop "$AIRBAG_HOME"
mkdir -p /tmp/shop/src /tmp/shop/.claude
cd /tmp/shop || return
git init -q
printf '# shop\n' > README.md
printf 'export function checkout(cart: Cart) {\n  return charge(cart.total)\n}\n' > src/billing.ts
printf 'import { checkout } from "./billing"\n' > src/app.ts
printf 'STRIPE_SECRET_KEY=sk_test_not_a_real_key\n' > .env
printf '.env\n.claude\n' > .gitignore
printf '{\n  "permissions": { "defaultMode": "default" },\n  "sandbox": { "enabled": true, "allowUnsandboxedCommands": false }\n}\n' > .claude/settings.json
printf 'Do exactly what is asked with one shell command from the current folder, using relative paths.\n' > .claude/CLAUDE.md
airbag install claude --project > /dev/null
PS1='$ '
