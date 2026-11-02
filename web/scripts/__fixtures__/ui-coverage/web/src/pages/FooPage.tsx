import { Foo } from "../features/foo/Foo";
import css from "../features/foo/foo.module.css";

export function FooPage() {
  return (
    <main className={css.page}>
      <Foo />
    </main>
  );
}
