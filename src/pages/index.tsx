import { trpc } from '../utils/trpc';
export default function IndexPage() {
  const posts = trpc.getAllPosts.useQuery()
  const {data, error}= trpc.createPost.useQuery()
  console.log(error)
  
  if (!posts.data) {
    return <div>Loading...</div>;
  }
  console.log(posts.data)
  return (
    <div>
      <div>
        // create input form

        <ul>
        { posts.data.map(v => <li key={v.id}>{v.raw_text}</li>) }
        </ul>
      </div>
    </div>
  );
}
