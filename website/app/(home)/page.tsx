import Link from 'next/link';

export default function HomePage() {
  return (
    <div className="flex flex-col justify-center text-center flex-1 gap-4 px-4">
      <h1 className="text-3xl font-bold">lait</h1>
      <p className="text-fd-muted-foreground">
        Lightweight AI Tool (lait) は、YAML で定義したハーネス、Agent Loop、Flow を CLI から実行・制御するためのツールです。
      </p>
      <p>
        <Link href="/docs" className="font-medium underline">
          ドキュメントを見る
        </Link>
      </p>
    </div>
  );
}
