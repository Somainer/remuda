import { helper } from "./helper";
import css from "./foo.module.css";

export function Foo() {
  return <p className={css.row}>{helper()}</p>;
}
