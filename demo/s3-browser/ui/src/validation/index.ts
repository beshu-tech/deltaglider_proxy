/**
 * The validation framework (UX harvest D9, D10, D46, D59):
 *  - `v.*`: pure validators, one per field kind (./validators.ts);
 *  - `useFieldValidation`: runs them on blur and on submit, gives
 *    FormField its `error`, the control `aria-invalid`, and the dirty bar
 *    its "N issues to fix" count; number fields keep the typed text.
 */
export * as v from './validators';
export { parseByteSize } from './validators';
export { useFieldValidation, type FieldValidation } from './useFieldValidation';
