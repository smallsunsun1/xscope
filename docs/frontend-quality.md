# Console localization and browser checks

The console supports `zh-CN` and `en-US`. Use the language selector in the header.
The first visit follows the browser language (Chinese or English fallback); an
explicit choice is saved as `xscope.locale` in localStorage and synchronized
between tabs. Storage restrictions do not prevent switching within a tab.
Changing language updates text and Ant Design locale without remounting forms.
HTML `lang`, page title, numbers, currency display, and dates follow the selection.
Currency and billing precision do not change when the language changes.

## Adding translated text

The typed catalog is `web/console/src/locales/messages.ts`: Chinese source text
is the key and English is the value. New display text must have both versions.
Call `useI18n()` in components that render translations so they subscribe to
language changes; use `t(key, values)` for text and interpolated values. Evaluate
translations during rendering, not in module-level objects that would retain
the language from the first page load. `format.ts` handles locale-aware amounts
and dates, including exact microunit strings.

Resource names, user input, model IDs, API identifiers, and backend diagnostics
are data, not translation keys. Common HTTP recovery messages are localized;
backend-specific validation messages remain the original response text.

## Running quality checks

All entry points use Bazel. Install the matching Playwright Chromium headless
browser once, and repeat after updating Playwright:

```sh
bazel run //web/console:install_browsers
bazel test //web/console:checks
```

`checks` includes application TypeScript checks, test TypeScript checks, exact currency formatting checks, and
Playwright. The browser tests depend on `//web/console:console` and serve its
production output on loopback port 4187. They mock API responses per test;
they do not log into Keycloak or change a Kubernetes cluster or its data.
Unexpected API calls fail the tests. Browser page exceptions also fail them.

Coverage includes all nine lazy-loaded pages in both languages, persisted and
cross-tab preferences, project creation/validation, exact billing amounts,
pending records, finance tabs, routing revision conflicts, member permissions,
unavailable services, model estimates, dialogs, and mobile navigation.

Playwright saves HTML reports, failure screenshots, and traces under:

```text
bazel-testlogs/web/console/e2e/test.outputs/
```

Browser tests use a versioned host browser cache and run locally without Bazel
sandboxing or remote execution; the `external` tag prevents stale test-result
caching. Set `PLAYWRIGHT_BROWSERS_PATH` consistently for browser installation and
testing when using a custom cache. Keep port 4187 available.

For Linux CI, provision the browser's OS dependencies (an appropriately configured
runner can use `bazel run //web/console:install_browsers -- --with-deps`), then run
the same `checks` target and upload `test.outputs` on failure. This is a frontend
regression suite; backend integration and real OIDC flows still require their
own cluster tests. Chromium desktop and mobile emulation are covered; Firefox
and WebKit are not part of this initial suite.
