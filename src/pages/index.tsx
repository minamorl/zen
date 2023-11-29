import { trpc } from '../utils/trpc';
export default function IndexPage() {
  const posts = trpc.getAllPosts.useQuery()
  
  if (!posts.data) {
    return <div>Loading...</div>;
  }
  console.log(posts.data)
  return (
    <div>
      <header>
        <h1>zen</h1>
        <div>A revolutional social media platform</div>
      </header>
      <div>
        <ul>
        { posts.data.map(v => <li>{v.raw_text}</li>) }
        </ul>
      </div>
    </div>
  );
}
