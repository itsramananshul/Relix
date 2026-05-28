/**
 * Jest configuration for the Relix TypeScript SDK.
 *
 * - ts-jest compiles `.ts` test files on the fly so we never need a
 *   pre-build step before running tests.
 * - testEnvironment: 'node' because the SDK targets Node 18+ and uses
 *   the native fetch API.
 * - --runInBand in package.json so the tests serialise across the
 *   global `fetch` mock and don't race each other.
 */
/** @type {import('jest').Config} */
module.exports = {
  preset: 'ts-jest',
  testEnvironment: 'node',
  testMatch: ['<rootDir>/tests/**/*.test.ts'],
  moduleFileExtensions: ['ts', 'js', 'json'],
  collectCoverageFrom: ['src/**/*.ts'],
  transform: {
    '^.+\\.ts$': ['ts-jest', { tsconfig: { sourceMap: false } }],
  },
};
