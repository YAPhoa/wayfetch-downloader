// How downloads go: directly from their hosts, or through a provider
// account selected automatically or explicitly at start. The engine
// decides which accounts may be used and keeps the choice with each job;
// these helpers only say what a view offers and shows. Neither ever holds
// a generated link or a credential.

/**
 * @typedef {import('./contract.js').Accounts} Accounts
 * @typedef {import('./contract.js').AccountSummary} AccountSummary
 * @typedef {import('./contract.js').Permission} Permission
 * @typedef {import('./generated/JobRoute.js').JobRoute} JobRoute
 */

/**
 * Whether a client with `permissions` can choose an account when starting
 * links: it needs `provider_downloads` to use one and `accounts` to see
 * which there are.
 * @param {readonly Permission[]} permissions
 */
export function canChooseRoute(permissions) {
  return permissions.includes('provider_downloads') && permissions.includes('accounts');
}

/**
 * The accounts downloads may go through: enabled ones of a service whose
 * downloads the engine supports, while the engine can open credentials.
 * The engine checks again when starting.
 * @param {Accounts | null} accounts
 * @returns {AccountSummary[]}
 */
export function routeAccounts(accounts) {
  if (!accounts || accounts.vault !== 'ready') return [];
  const downloads = new Set(accounts.services.filter((s) => s.downloads).map((s) => s.service));
  return accounts.accounts.filter((a) => a.enabled && downloads.has(a.service));
}

/**
 * An account's name: its alias, else the provider's user name.
 * @param {AccountSummary} account
 * @param {string} service the service's name
 */
export function accountName(account, service) {
  return account.alias ?? account.username ?? `${service} account`;
}

/**
 * How a job downloads, in words: "Direct", or the service and account it
 * goes through. An account removed since is said to be; without the
 * accounts list (no `accounts` permission) only the service is named.
 * @param {JobRoute | undefined} route
 * @param {Accounts | null} accounts
 * @param {(service: string) => string} serviceName
 */
export function routeLabel(route, accounts, serviceName) {
  if (!route) return 'Direct';
  const service = serviceName(route.service);
  if (!accounts) return service;
  const account = accounts.accounts.find((a) => a.id === route.account);
  return account ? `${service}, ${accountName(account, service)}` : `${service}, removed account`;
}
